//! The mounting side of a served drive: a `fuser` filesystem that turns
//! kernel calls into protocol requests against the serving node.
//!
//! `fuser`'s `Filesystem` callbacks are synchronous and run on a plain
//! background thread (not a tokio task), so each one bridges into the
//! async protocol with `Handle::block_on`. A request that can't reach the
//! server, whether the connection was never open, dropped mid-call, or
//! simply times out, comes back as `EIO`: a stuck syscall on every process
//! touching the mount is worse than a filesystem error, and `EIO` is what a
//! real disk failure looks like to userspace too. The next call reopens the
//! connection on its own; there is no separate "reconnect" step to run.
//!
//! Attributes and directory listings are cached briefly (`ClientConfig::ttl`)
//! so that walking a tree doesn't round-trip to another machine for every
//! `getattr`. This is why a served drive is described as consistent only up
//! to that TTL from another mount's point of view: a write on the server
//! (or through a different mount) can take up to `ttl` to become visible
//! here.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::{
    Errno, FileHandle, FileType as FuseFileType, Filesystem, FopenFlags, Generation, INodeNo,
    OpenAccMode, OpenFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request as FuseRequest, TimeOrNow,
};
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;
use tracing::debug;

use crate::served::protocol::{self, Attr, DirEntry, FileKind, ProtoError, Request, Response};
use crate::transport::{frame, Connection};

const ROOT_INO: u64 = 1;

/// Tuning for one mount. Kept small; the more interesting values (server
/// address) go through `ClientFs::new` directly.
#[derive(Debug, Clone, Copy)]
pub struct ClientConfig {
    /// How long a cached attribute or directory listing is trusted before
    /// the client asks the server again. Also the TTL handed back to the
    /// kernel in FUSE replies, so the kernel's own cache expires in step
    /// with ours.
    pub ttl: Duration,
    /// How long to wait for a TCP connect to the serving node.
    pub connect_timeout: Duration,
    /// How long to wait for a single request's response before giving up
    /// and reporting `EIO`. Bounds the worst case when the server accepted
    /// the connection but has stopped answering.
    pub request_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(3),
            request_timeout: Duration::from_secs(10),
        }
    }
}

fn child_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// Bidirectional map between the inode numbers the kernel deals in and the
/// paths the wire protocol deals in. The kernel needs a stable inode number
/// per file for its own cache to behave; the server has no concept of
/// inodes at all, so the client is the only place this mapping can live.
struct Inodes {
    next: u64,
    path_to_ino: HashMap<String, u64>,
    ino_to_path: HashMap<u64, String>,
}

impl Inodes {
    fn new() -> Self {
        let mut path_to_ino = HashMap::new();
        let mut ino_to_path = HashMap::new();
        path_to_ino.insert(String::new(), ROOT_INO);
        ino_to_path.insert(ROOT_INO, String::new());
        Self { next: ROOT_INO + 1, path_to_ino, ino_to_path }
    }

    fn ino_for(&mut self, path: &str) -> u64 {
        if let Some(&ino) = self.path_to_ino.get(path) {
            return ino;
        }
        let ino = self.next;
        self.next += 1;
        self.path_to_ino.insert(path.to_string(), ino);
        self.ino_to_path.insert(ino, path.to_string());
        ino
    }

    fn path_of(&self, ino: u64) -> Option<String> {
        self.ino_to_path.get(&ino).cloned()
    }

    /// Repoint every inode under `from` (itself included) at the equivalent
    /// path under `to`, so a renamed directory keeps its children's inode
    /// numbers instead of the kernel seeing them as freshly created.
    fn rename(&mut self, from: &str, to: &str) {
        let prefix = format!("{from}/");
        let affected: Vec<String> =
            self.path_to_ino.keys().filter(|p| p.as_str() == from || p.starts_with(&prefix)).cloned().collect();
        for old in affected {
            let Some(ino) = self.path_to_ino.remove(&old) else { continue };
            let new = if old == from { to.to_string() } else { format!("{to}{}", &old[from.len()..]) };
            self.ino_to_path.insert(ino, new.clone());
            self.path_to_ino.insert(new, ino);
        }
    }
}

struct Cached<T> {
    value: T,
    expires: Instant,
}

/// A small TTL cache keyed by path. Separate instances back attributes and
/// directory listings since they're invalidated independently.
struct Cache<T> {
    ttl: Duration,
    entries: HashMap<String, Cached<T>>,
}

impl<T: Clone> Cache<T> {
    fn new(ttl: Duration) -> Self {
        Self { ttl, entries: HashMap::new() }
    }

    fn get(&self, key: &str) -> Option<T> {
        let entry = self.entries.get(key)?;
        (entry.expires > Instant::now()).then(|| entry.value.clone())
    }

    fn put(&mut self, key: &str, value: T) {
        self.entries.insert(key.to_string(), Cached { value, expires: Instant::now() + self.ttl });
    }

    fn invalidate(&mut self, key: &str) {
        self.entries.remove(key);
    }
}

/// FUSE filesystem for a mounted served drive.
pub struct ClientFs {
    /// Shared so a failover watcher can repoint this mount at a new server
    /// after the previous one drops out, without needing to reach back
    /// through `fuser::spawn_mount`, which takes ownership of `self`.
    server: Arc<Mutex<SocketAddr>>,
    rt: tokio::runtime::Handle,
    conn: AsyncMutex<Option<Connection>>,
    config: ClientConfig,
    inodes: Mutex<Inodes>,
    attrs: Mutex<Cache<Attr>>,
    dirs: Mutex<Cache<Vec<DirEntry>>>,
}

impl ClientFs {
    /// `rt` must belong to a multi-threaded runtime. Each FUSE callback
    /// blocks its own background thread on `rt.block_on`, which needs a
    /// worker thread free to actually drive the request; a current-thread
    /// runtime has only the one thread, and it's typically busy elsewhere,
    /// so `block_on` from here would deadlock against it.
    pub fn new(server: SocketAddr, rt: tokio::runtime::Handle) -> Self {
        Self::with_config(server, rt, ClientConfig::default())
    }

    pub fn with_config(server: SocketAddr, rt: tokio::runtime::Handle, config: ClientConfig) -> Self {
        Self {
            server: Arc::new(Mutex::new(server)),
            rt,
            conn: AsyncMutex::new(None),
            attrs: Mutex::new(Cache::new(config.ttl)),
            dirs: Mutex::new(Cache::new(config.ttl)),
            config,
            inodes: Mutex::new(Inodes::new()),
        }
    }

    /// A handle to the server address this mount is currently pointed at,
    /// so the caller can retarget it after handing `self` to
    /// `fuser::spawn_mount` (which takes ownership) once a served drive's
    /// active server changes.
    pub fn server_handle(&self) -> Arc<Mutex<SocketAddr>> {
        self.server.clone()
    }

    fn path_of(&self, ino: u64) -> Option<String> {
        self.inodes.lock().expect("inode table lock").path_of(ino)
    }

    fn ino_for(&self, path: &str) -> u64 {
        self.inodes.lock().expect("inode table lock").ino_for(path)
    }

    fn rename_inode(&self, from: &str, to: &str) {
        self.inodes.lock().expect("inode table lock").rename(from, to);
    }

    fn cached_attr(&self, path: &str) -> Option<Attr> {
        self.attrs.lock().expect("attr cache lock").get(path)
    }

    fn cache_attr(&self, path: &str, attr: Attr) {
        self.attrs.lock().expect("attr cache lock").put(path, attr);
    }

    fn cached_dir(&self, path: &str) -> Option<Vec<DirEntry>> {
        self.dirs.lock().expect("dir cache lock").get(path)
    }

    fn cache_dir(&self, path: &str, entries: Vec<DirEntry>) {
        self.dirs.lock().expect("dir cache lock").put(path, entries);
    }

    /// Drop anything cached about `path`, so the next lookup asks the
    /// server again. Called after any operation that changes it locally.
    fn invalidate(&self, path: &str) {
        self.attrs.lock().expect("attr cache lock").invalidate(path);
        self.dirs.lock().expect("dir cache lock").invalidate(path);
    }

    async fn connect(&self) -> io::Result<Connection> {
        let server = *self.server.lock().expect("server addr lock");
        let stream = tokio::time::timeout(self.config.connect_timeout, TcpStream::connect(server))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect to served drive timed out"))??;
        Ok(frame(stream))
    }

    async fn call_async(&self, req: Request) -> io::Result<Response> {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            *guard = Some(self.connect().await?);
        }
        let conn = guard.as_mut().expect("just filled");

        if protocol::send_request(conn, &req).await.is_err() {
            *guard = None;
            return Err(io::Error::other("served drive connection lost"));
        }
        match protocol::recv_response(conn).await {
            Ok(Some(resp)) => Ok(resp),
            Ok(None) => {
                *guard = None;
                Err(io::Error::other("served drive closed the connection"))
            }
            Err(e) => {
                *guard = None;
                Err(e)
            }
        }
    }

    /// Run one request against the server, bridging into the async world.
    /// Any failure to reach the server, including a timeout, becomes `EIO`.
    /// A blocking call here is safe: `fuser::spawn_mount` runs each
    /// filesystem callback on its own plain thread, never on a tokio task.
    fn request(&self, req: Request) -> Result<Response, Errno> {
        let timeout = self.config.request_timeout;
        let result = self.rt.block_on(async { tokio::time::timeout(timeout, self.call_async(req)).await });
        match result {
            Ok(Ok(Response::Err(e))) => Err(proto_errno(&e)),
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(e)) => {
                debug!(error = %e, "served drive request failed");
                Err(Errno::EIO)
            }
            Err(_timed_out) => {
                debug!("served drive request timed out");
                Err(Errno::EIO)
            }
        }
    }
}

fn proto_errno(e: &ProtoError) -> Errno {
    match e {
        ProtoError::NotFound => Errno::ENOENT,
        ProtoError::NotDir => Errno::ENOTDIR,
        ProtoError::IsDir => Errno::EISDIR,
        ProtoError::NotEmpty => Errno::ENOTEMPTY,
        ProtoError::AlreadyExists => Errno::EEXIST,
        ProtoError::PermissionDenied => Errno::EACCES,
        ProtoError::InvalidHandle => Errno::EBADF,
        ProtoError::Escape => Errno::EACCES,
        ProtoError::Io(_) => Errno::EIO,
    }
}

fn fuse_kind(kind: FileKind) -> FuseFileType {
    match kind {
        FileKind::File => FuseFileType::RegularFile,
        FileKind::Dir => FuseFileType::Directory,
        FileKind::Symlink => FuseFileType::Symlink,
    }
}

fn nanos_to_systemtime(nanos: i64) -> SystemTime {
    if nanos >= 0 {
        UNIX_EPOCH + Duration::from_nanos(nanos as u64)
    } else {
        UNIX_EPOCH - Duration::from_nanos((-nanos) as u64)
    }
}

fn systemtime_to_nanos(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

fn time_or_now_to_nanos(t: TimeOrNow) -> i64 {
    match t {
        TimeOrNow::Now => systemtime_to_nanos(SystemTime::now()),
        TimeOrNow::SpecificTime(t) => systemtime_to_nanos(t),
    }
}

/// This process's own uid/gid stand in for every file's owner: the protocol
/// carries permission bits but not ownership, since a served drive is
/// meant to be read by whatever uid the containers on each node run as,
/// same as a bind mount would present it.
fn fuse_attr(ino: u64, attr: &Attr) -> fuser::FileAttr {
    fuser::FileAttr {
        ino: INodeNo(ino),
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: nanos_to_systemtime(attr.atime_nanos),
        mtime: nanos_to_systemtime(attr.mtime_nanos),
        ctime: nanos_to_systemtime(attr.mtime_nanos),
        crtime: UNIX_EPOCH,
        kind: fuse_kind(attr.kind),
        perm: (attr.mode & 0o7777) as u16,
        nlink: attr.nlink,
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

impl Filesystem for ClientFs {
    fn lookup(&self, _req: &FuseRequest, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_path) = self.path_of(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = child_path(&parent_path, name);

        if let Some(attr) = self.cached_attr(&path) {
            let ino = self.ino_for(&path);
            reply.entry(&self.config.ttl, &fuse_attr(ino, &attr), Generation(0));
            return;
        }

        match self.request(Request::Lookup(path.clone())) {
            Ok(Response::Attr(attr)) => {
                self.cache_attr(&path, attr.clone());
                let ino = self.ino_for(&path);
                reply.entry(&self.config.ttl, &fuse_attr(ino, &attr), Generation(0));
            }
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &FuseRequest, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(path) = self.path_of(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if let Some(attr) = self.cached_attr(&path) {
            reply.attr(&self.config.ttl, &fuse_attr(ino.0, &attr));
            return;
        }
        match self.request(Request::GetAttr(path.clone())) {
            Ok(Response::Attr(attr)) => {
                self.cache_attr(&path, attr.clone());
                reply.attr(&self.config.ttl, &fuse_attr(ino.0, &attr));
            }
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _req: &FuseRequest,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let Some(path) = self.path_of(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        if let Some(size) = size {
            if let Err(e) = self.request(Request::Truncate { path: path.clone(), size }) {
                reply.error(e);
                return;
            }
        }

        let mtime_nanos = mtime.map(time_or_now_to_nanos);
        let atime_nanos = atime.map(time_or_now_to_nanos);
        let attr = if mode.is_some() || mtime_nanos.is_some() || atime_nanos.is_some() {
            self.request(Request::SetAttr { path: path.clone(), mode, mtime_nanos, atime_nanos })
        } else {
            self.request(Request::GetAttr(path.clone()))
        };

        match attr {
            Ok(Response::Attr(attr)) => {
                self.invalidate(&path);
                self.cache_attr(&path, attr.clone());
                reply.attr(&self.config.ttl, &fuse_attr(ino.0, &attr));
            }
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &FuseRequest, ino: INodeNo, reply: ReplyData) {
        let Some(path) = self.path_of(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.request(Request::Readlink(path)) {
            Ok(Response::Link(target)) => reply.data(target.as_bytes()),
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &self,
        _req: &FuseRequest,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(parent_path) = self.path_of(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = child_path(&parent_path, name);

        if let Err(e) = self.request(Request::Mkdir { path: path.clone(), mode }) {
            reply.error(e);
            return;
        }
        self.invalidate(&parent_path);
        match self.request(Request::GetAttr(path.clone())) {
            Ok(Response::Attr(attr)) => {
                self.cache_attr(&path, attr.clone());
                let ino = self.ino_for(&path);
                reply.entry(&self.config.ttl, &fuse_attr(ino, &attr), Generation(0));
            }
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &FuseRequest, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_path) = self.path_of(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = child_path(&parent_path, name);
        match self.request(Request::Unlink(path.clone())) {
            Ok(Response::Ok) => {
                self.invalidate(&path);
                self.invalidate(&parent_path);
                reply.ok();
            }
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, _req: &FuseRequest, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_path) = self.path_of(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = child_path(&parent_path, name);
        match self.request(Request::Rmdir(path.clone())) {
            Ok(Response::Ok) => {
                self.invalidate(&path);
                self.invalidate(&parent_path);
                reply.ok();
            }
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }

    fn symlink(
        &self,
        _req: &FuseRequest,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let Some(parent_path) = self.path_of(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = link_name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = child_path(&parent_path, name);
        let target = target.to_string_lossy().into_owned();

        if let Err(e) = self.request(Request::Symlink { path: path.clone(), target }) {
            reply.error(e);
            return;
        }
        self.invalidate(&parent_path);
        match self.request(Request::GetAttr(path.clone())) {
            Ok(Response::Attr(attr)) => {
                self.cache_attr(&path, attr.clone());
                let ino = self.ino_for(&path);
                reply.entry(&self.config.ttl, &fuse_attr(ino, &attr), Generation(0));
            }
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &self,
        _req: &FuseRequest,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (Some(parent_path), Some(newparent_path)) = (self.path_of(parent.0), self.path_of(newparent.0)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let from = child_path(&parent_path, name);
        let to = child_path(&newparent_path, newname);

        match self.request(Request::Rename { from: from.clone(), to: to.clone() }) {
            Ok(Response::Ok) => {
                self.rename_inode(&from, &to);
                self.invalidate(&from);
                self.invalidate(&to);
                self.invalidate(&parent_path);
                self.invalidate(&newparent_path);
                reply.ok();
            }
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }

    fn link(&self, _req: &FuseRequest, ino: INodeNo, newparent: INodeNo, newname: &OsStr, reply: ReplyEntry) {
        let (Some(target_path), Some(newparent_path)) = (self.path_of(ino.0), self.path_of(newparent.0)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(newname) = newname.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = child_path(&newparent_path, newname);

        if let Err(e) = self.request(Request::Link { path: path.clone(), target: target_path }) {
            reply.error(e);
            return;
        }
        self.invalidate(&newparent_path);
        match self.request(Request::GetAttr(path.clone())) {
            Ok(Response::Attr(attr)) => {
                self.cache_attr(&path, attr.clone());
                let ino = self.ino_for(&path);
                reply.entry(&self.config.ttl, &fuse_attr(ino, &attr), Generation(0));
            }
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, _req: &FuseRequest, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let Some(path) = self.path_of(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let write = flags.acc_mode() != OpenAccMode::O_RDONLY;
        match self.request(Request::Open { path, write }) {
            Ok(Response::Opened(handle)) => reply.opened(FileHandle(handle), FopenFlags::empty()),
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &FuseRequest,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        match self.request(Request::Read { handle: fh.0, offset, len: size }) {
            Ok(Response::Data(data)) => reply.data(&data),
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &FuseRequest,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.request(Request::Write { handle: fh.0, offset, data: data.to_vec() }) {
            Ok(Response::Written(n)) => reply.written(n),
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }

    fn release(
        &self,
        _req: &FuseRequest,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // Best effort: the file descriptor is already gone from the kernel's
        // point of view either way, and there is nothing useful to retry.
        let _ = self.request(Request::Release(fh.0));
        reply.ok();
    }

    fn fsync(&self, _req: &FuseRequest, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        match self.request(Request::Fsync(fh.0)) {
            Ok(Response::Ok) => reply.ok(),
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }

    fn create(
        &self,
        _req: &FuseRequest,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(parent_path) = self.path_of(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = child_path(&parent_path, name);

        match self.request(Request::Create { path: path.clone(), mode }) {
            Ok(Response::Created { handle, attr }) => {
                self.invalidate(&parent_path);
                self.cache_attr(&path, attr.clone());
                let ino = self.ino_for(&path);
                reply.created(&self.config.ttl, &fuse_attr(ino, &attr), Generation(0), FileHandle(handle), FopenFlags::empty());
            }
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(&self, _req: &FuseRequest, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let Some(path) = self.path_of(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let entries = match self.cached_dir(&path) {
            Some(e) => e,
            None => match self.request(Request::ReadDir(path.clone())) {
                Ok(Response::Entries(e)) => {
                    self.cache_dir(&path, e.clone());
                    e
                }
                Ok(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
                Err(e) => {
                    reply.error(e);
                    return;
                }
            },
        };

        let mut all: Vec<(String, FuseFileType)> =
            vec![(".".to_string(), FuseFileType::Directory), ("..".to_string(), FuseFileType::Directory)];
        for entry in &entries {
            all.push((entry.name.clone(), fuse_kind(entry.kind)));
        }

        for (i, (name, kind)) in all.iter().enumerate().skip(offset as usize) {
            let child_ino = if name == "." || name == ".." { ino.0 } else { self.ino_for(&child_path(&path, name)) };
            let next_offset = (i + 1) as u64;
            if reply.add(INodeNo(child_ino), next_offset, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &FuseRequest, _ino: INodeNo, reply: ReplyStatfs) {
        match self.request(Request::Statfs) {
            Ok(Response::Statfs(s)) => reply.statfs(s.blocks, s.bfree, s.bavail, s.files, s.ffree, s.bsize, s.namelen, s.bsize),
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_inode_is_one_and_maps_to_the_empty_path() {
        let inodes = Inodes::new();
        assert_eq!(inodes.path_of(ROOT_INO), Some(String::new()));
    }

    #[test]
    fn ino_for_is_stable_for_the_same_path() {
        let mut inodes = Inodes::new();
        let a = inodes.ino_for("a.txt");
        let b = inodes.ino_for("a.txt");
        assert_eq!(a, b);
        let c = inodes.ino_for("b.txt");
        assert_ne!(a, c);
    }

    #[test]
    fn rename_moves_the_inode_to_the_new_path() {
        let mut inodes = Inodes::new();
        let ino = inodes.ino_for("old.txt");
        inodes.rename("old.txt", "new.txt");
        assert_eq!(inodes.path_of(ino), Some("new.txt".to_string()));
        assert_eq!(inodes.ino_for("new.txt"), ino);
    }

    #[test]
    fn rename_moves_children_of_a_renamed_directory() {
        let mut inodes = Inodes::new();
        let dir_ino = inodes.ino_for("dir");
        let child_ino = inodes.ino_for("dir/child.txt");
        inodes.rename("dir", "moved");
        assert_eq!(inodes.path_of(dir_ino), Some("moved".to_string()));
        assert_eq!(inodes.path_of(child_ino), Some("moved/child.txt".to_string()));
    }

    #[test]
    fn cache_returns_none_once_expired() {
        let mut cache = Cache::new(Duration::from_millis(5));
        cache.put("a", 1u32);
        assert_eq!(cache.get("a"), Some(1));
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(cache.get("a"), None);
    }

    #[test]
    fn cache_invalidate_removes_the_entry_immediately() {
        let mut cache = Cache::new(Duration::from_secs(60));
        cache.put("a", 1u32);
        cache.invalidate("a");
        assert_eq!(cache.get("a"), None);
    }

    #[test]
    fn child_path_joins_with_a_slash_except_at_the_root() {
        assert_eq!(child_path("", "a.txt"), "a.txt");
        assert_eq!(child_path("dir", "a.txt"), "dir/a.txt");
    }

    #[test]
    fn proto_error_maps_to_the_matching_errno() {
        assert_eq!(proto_errno(&ProtoError::NotFound), Errno::ENOENT);
        assert_eq!(proto_errno(&ProtoError::IsDir), Errno::EISDIR);
        assert_eq!(proto_errno(&ProtoError::Escape), Errno::EACCES);
    }

    #[test]
    fn nanos_round_trip_through_systemtime() {
        let nanos = 1_700_000_000_123_456_789i64;
        assert_eq!(systemtime_to_nanos(nanos_to_systemtime(nanos)), nanos);
    }
}
