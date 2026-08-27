//! The serving side of a served drive: answers the protocol against the
//! real directory, for every FUSE mount elsewhere in the cluster.
//!
//! One TCP connection per mount. Requests on a connection are handled one
//! at a time and in order, same as a real filesystem call would be from a
//! single thread, so a handle table doesn't need its own lock: it lives for
//! exactly as long as the connection that owns it, and drops (closing every
//! open file) the moment that connection ends. That's also what makes
//! disconnect cleanup free: a client going away just ends the task, and the
//! table's `Drop` does the rest.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::{TcpListener, TcpStream};
use tracing::debug;

use crate::scan::nanos_to_filetime;

use crate::registry::resolve_in_root;
use crate::served::protocol::{
    self, Attr, DirEntry, FileKind, Handle, ProtoError, Request, Response, StatfsInfo,
};
use crate::transport::frame;

/// Open file handles for one connection. Not `Send`-shared: each connection
/// task owns its own table.
#[derive(Default)]
pub struct HandleTable {
    next: u64,
    open: HashMap<Handle, std::fs::File>,
}

impl HandleTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, file: std::fs::File) -> Handle {
        self.next += 1;
        let handle = self.next;
        self.open.insert(handle, file);
        handle
    }

    pub fn get(&self, handle: Handle) -> Option<&std::fs::File> {
        self.open.get(&handle)
    }

    pub fn close(&mut self, handle: Handle) -> Option<std::fs::File> {
        self.open.remove(&handle)
    }

    pub fn len(&self) -> usize {
        self.open.len()
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }
}

/// Accept connections and serve `root` until the task is cancelled.
pub async fn serve(addr: SocketAddr, root: PathBuf) -> io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    debug!(%addr, "served drive listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        let root = root.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, root).await {
                debug!(%peer, error = %e, "served drive connection closed");
            }
        });
    }
}

/// Run one connection until the client disconnects or the connection
/// errors. Exposed directly so tests can drive it over a loopback pair.
pub async fn handle_connection(stream: TcpStream, root: PathBuf) -> io::Result<()> {
    let mut conn = frame(stream);
    let mut handles = HandleTable::new();

    while let Some(req) = protocol::recv_request(&mut conn).await? {
        let resp = dispatch(&root, &mut handles, req);
        protocol::send_response(&mut conn, &resp).await?;
    }

    Ok(())
    // `handles` drops here, closing every file still open on this connection.
}

fn dispatch(root: &Path, handles: &mut HandleTable, req: Request) -> Response {
    match req {
        Request::Lookup(path) | Request::GetAttr(path) => attr(root, &path),
        Request::SetAttr { path, mode, mtime_nanos, atime_nanos } => {
            set_attr(root, &path, mode, mtime_nanos, atime_nanos)
        }
        Request::ReadDir(path) => read_dir(root, &path),
        Request::Open { path, write } => open(root, handles, &path, write),
        Request::Read { handle, offset, len } => read(handles, handle, offset, len),
        Request::Write { handle, offset, data } => write(handles, handle, offset, &data),
        Request::Create { path, mode } => create(root, handles, &path, mode),
        Request::Mkdir { path, mode } => mkdir(root, &path, mode),
        Request::Unlink(path) => remove(root, &path, false),
        Request::Rmdir(path) => remove(root, &path, true),
        Request::Rename { from, to } => rename(root, &from, &to),
        Request::Truncate { path, size } => truncate(root, &path, size),
        Request::Fsync(handle) => fsync(handles, handle),
        Request::Release(handle) => {
            handles.close(handle);
            Response::Ok
        }
        Request::Statfs => statfs(root),
        Request::Symlink { path, target } => symlink(root, &path, &target),
        Request::Readlink(path) => readlink(root, &path),
        Request::Link { path, target } => link(root, &path, &target),
    }
}

fn err_resp(e: crate::error::DriveError) -> Response {
    match e {
        crate::error::DriveError::PathEscape { .. } => Response::Err(ProtoError::Escape),
        crate::error::DriveError::Io { source, .. } => Response::Err(ProtoError::from_io(&source)),
        other => Response::Err(ProtoError::Io(other.to_string())),
    }
}

/// Resolve `rel` under `root`. `""` and `"."` both mean the drive root
/// itself: the client sends one of these for the root directory, since it
/// has no parent name to give.
fn resolve(root: &Path, rel: &str) -> Result<PathBuf, Response> {
    if rel.is_empty() || rel == "." {
        return Ok(root.to_path_buf());
    }
    resolve_in_root(root, rel).map_err(err_resp)
}

fn attr_of(path: &Path) -> io::Result<Attr> {
    let meta = std::fs::symlink_metadata(path)?;
    let kind = if meta.file_type().is_symlink() {
        FileKind::Symlink
    } else if meta.is_dir() {
        FileKind::Dir
    } else {
        FileKind::File
    };
    Ok(Attr {
        kind,
        size: meta.size(),
        mode: meta.mode() & 0o7777,
        mtime_nanos: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
        atime_nanos: meta.atime() * 1_000_000_000 + meta.atime_nsec(),
        nlink: meta.nlink() as u32,
    })
}

fn attr(root: &Path, rel: &str) -> Response {
    let path = match resolve(root, rel) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match attr_of(&path) {
        Ok(a) => Response::Attr(a),
        Err(e) => Response::Err(ProtoError::from_io(&e)),
    }
}

fn set_attr(root: &Path, rel: &str, mode: Option<u32>, mtime_nanos: Option<i64>, atime_nanos: Option<i64>) -> Response {
    let path = match resolve(root, rel) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Some(mode) = mode {
        if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)) {
            return Response::Err(ProtoError::from_io(&e));
        }
    }
    if mtime_nanos.is_some() || atime_nanos.is_some() {
        let current = match attr_of(&path) {
            Ok(a) => a,
            Err(e) => return Response::Err(ProtoError::from_io(&e)),
        };
        let mtime = nanos_to_filetime(mtime_nanos.unwrap_or(current.mtime_nanos));
        let atime = nanos_to_filetime(atime_nanos.unwrap_or(current.atime_nanos));
        if let Err(e) = filetime::set_file_times(&path, atime, mtime) {
            return Response::Err(ProtoError::Io(e.to_string()));
        }
    }
    attr(root, rel)
}

fn read_dir(root: &Path, rel: &str) -> Response {
    let path = match resolve(root, rel) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let entries = match std::fs::read_dir(&path) {
        Ok(rd) => rd,
        Err(e) => return Response::Err(ProtoError::from_io(&e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => return Response::Err(ProtoError::from_io(&e)),
        };
        let meta = match std::fs::symlink_metadata(entry.path()) {
            Ok(m) => m,
            Err(e) => return Response::Err(ProtoError::from_io(&e)),
        };
        let kind = if meta.file_type().is_symlink() {
            FileKind::Symlink
        } else if meta.is_dir() {
            FileKind::Dir
        } else {
            FileKind::File
        };
        out.push(DirEntry { name: entry.file_name().to_string_lossy().into_owned(), kind });
    }
    Response::Entries(out)
}

fn open(root: &Path, handles: &mut HandleTable, rel: &str, write: bool) -> Response {
    let path = match resolve(root, rel) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let opened = std::fs::OpenOptions::new().read(true).write(write).open(&path);
    match opened {
        Ok(file) => Response::Opened(handles.insert(file)),
        Err(e) => Response::Err(ProtoError::from_io(&e)),
    }
}

fn create(root: &Path, handles: &mut HandleTable, rel: &str, mode: u32) -> Response {
    let path = match resolve(root, rel) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(mode)
        .open(&path);
    let file = match opened {
        Ok(f) => f,
        Err(e) => return Response::Err(ProtoError::from_io(&e)),
    };
    let attr = match attr_of(&path) {
        Ok(a) => a,
        Err(e) => return Response::Err(ProtoError::from_io(&e)),
    };
    Response::Created { handle: handles.insert(file), attr }
}

/// Read at most `len` bytes, chunked so a caller moving a whole file never
/// has to fit it in a single frame: `MAX_CHUNK` bounds the value the client
/// ever asks for in one request.
fn read(handles: &HandleTable, handle: Handle, offset: u64, len: u32) -> Response {
    use std::io::{Read, Seek, SeekFrom};
    let Some(file) = handles.get(handle) else {
        return Response::Err(ProtoError::InvalidHandle);
    };
    let mut file = file.try_clone().expect("dup fd for read");
    let len = len.min(protocol::MAX_CHUNK) as usize;
    let mut buf = vec![0u8; len];
    if let Err(e) = file.seek(SeekFrom::Start(offset)) {
        return Response::Err(ProtoError::from_io(&e));
    }
    let mut read_total = 0;
    loop {
        match file.read(&mut buf[read_total..]) {
            Ok(0) => break,
            Ok(n) => read_total += n,
            Err(e) => return Response::Err(ProtoError::from_io(&e)),
        }
    }
    buf.truncate(read_total);
    Response::Data(buf)
}

fn write(handles: &HandleTable, handle: Handle, offset: u64, data: &[u8]) -> Response {
    use std::io::{Seek, SeekFrom, Write as _};
    let Some(file) = handles.get(handle) else {
        return Response::Err(ProtoError::InvalidHandle);
    };
    let mut file = file.try_clone().expect("dup fd for write");
    if let Err(e) = file.seek(SeekFrom::Start(offset)) {
        return Response::Err(ProtoError::from_io(&e));
    }
    if let Err(e) = file.write_all(data) {
        return Response::Err(ProtoError::from_io(&e));
    }
    Response::Written(data.len() as u32)
}

fn mkdir(root: &Path, rel: &str, mode: u32) -> Response {
    let path = match resolve(root, rel) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Err(e) = std::fs::create_dir(&path) {
        return Response::Err(ProtoError::from_io(&e));
    }
    if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)) {
        return Response::Err(ProtoError::from_io(&e));
    }
    Response::Ok
}

fn remove(root: &Path, rel: &str, dir: bool) -> Response {
    let path = match resolve(root, rel) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let result = if dir { std::fs::remove_dir(&path) } else { std::fs::remove_file(&path) };
    match result {
        Ok(()) => Response::Ok,
        Err(e) => Response::Err(ProtoError::from_io(&e)),
    }
}

fn rename(root: &Path, from: &str, to: &str) -> Response {
    let src = match resolve(root, from) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let dst = match resolve(root, to) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::rename(&src, &dst) {
        Ok(()) => Response::Ok,
        Err(e) => Response::Err(ProtoError::from_io(&e)),
    }
}

fn truncate(root: &Path, rel: &str, size: u64) -> Response {
    let path = match resolve(root, rel) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let file = match std::fs::OpenOptions::new().write(true).open(&path) {
        Ok(f) => f,
        Err(e) => return Response::Err(ProtoError::from_io(&e)),
    };
    match file.set_len(size) {
        Ok(()) => Response::Ok,
        Err(e) => Response::Err(ProtoError::from_io(&e)),
    }
}

fn fsync(handles: &HandleTable, handle: Handle) -> Response {
    let Some(file) = handles.get(handle) else {
        return Response::Err(ProtoError::InvalidHandle);
    };
    match file.sync_all() {
        Ok(()) => Response::Ok,
        Err(e) => Response::Err(ProtoError::from_io(&e)),
    }
}

fn statfs(root: &Path) -> Response {
    let c_path = match std::ffi::CString::new(root.as_os_str().as_encoded_bytes()) {
        Ok(c) => c,
        Err(_) => return Response::Err(ProtoError::Io("path has embedded NUL".into())),
    };
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Response::Err(ProtoError::from_io(&io::Error::last_os_error()));
    }
    Response::Statfs(StatfsInfo {
        blocks: stat.f_blocks as u64,
        bfree: stat.f_bfree as u64,
        bavail: stat.f_bavail as u64,
        files: stat.f_files as u64,
        ffree: stat.f_ffree as u64,
        bsize: stat.f_frsize as u32,
        namelen: stat.f_namemax as u32,
    })
}

fn symlink(root: &Path, rel: &str, target: &str) -> Response {
    // The target text is not itself resolved under the drive root: like any
    // real filesystem, a symlink can point anywhere, including outside the
    // drive or nowhere at all. What matters is that following it back
    // through this protocol later goes through `resolve_in_root` again,
    // which is what keeps a read or write from ever landing outside root.
    let path = match resolve(root, rel) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::os::unix::fs::symlink(target, &path) {
        Ok(()) => Response::Ok,
        Err(e) => Response::Err(ProtoError::from_io(&e)),
    }
}

fn readlink(root: &Path, rel: &str) -> Response {
    let path = match resolve(root, rel) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::read_link(&path) {
        Ok(target) => Response::Link(target.to_string_lossy().into_owned()),
        Err(e) => Response::Err(ProtoError::from_io(&e)),
    }
}

fn link(root: &Path, rel: &str, target: &str) -> Response {
    let path = match resolve(root, rel) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let target_path = match resolve(root, target) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::hard_link(&target_path, &path) {
        Ok(()) => Response::Ok,
        Err(e) => Response::Err(ProtoError::from_io(&e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_table_assigns_increasing_handles_and_closes_them() {
        let mut table = HandleTable::new();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let f1 = std::fs::File::open(tmp.path()).unwrap();
        let f2 = std::fs::File::open(tmp.path()).unwrap();

        let h1 = table.insert(f1);
        let h2 = table.insert(f2);
        assert_ne!(h1, h2);
        assert_eq!(table.len(), 2);

        assert!(table.get(h1).is_some());
        assert!(table.close(h1).is_some());
        assert!(table.get(h1).is_none());
        assert_eq!(table.len(), 1);

        assert!(table.close(999).is_none(), "closing an unknown handle is a no-op, not a panic");
    }

    #[test]
    fn handle_table_is_empty_reports_correctly() {
        let mut table = HandleTable::new();
        assert!(table.is_empty());
        let tmp = tempfile::NamedTempFile::new().unwrap();
        table.insert(std::fs::File::open(tmp.path()).unwrap());
        assert!(!table.is_empty());
    }

    #[test]
    fn dispatch_rejects_a_dot_dot_path() {
        let tmp = tempfile::tempdir().unwrap();
        let mut handles = HandleTable::new();
        let resp = dispatch(tmp.path(), &mut handles, Request::GetAttr("../escape".into()));
        assert_eq!(resp, Response::Err(ProtoError::Escape));
    }

    #[test]
    fn dispatch_rejects_a_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"shadow").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let mut handles = HandleTable::new();
        let resp = dispatch(&root, &mut handles, Request::GetAttr("escape/secret.txt".into()));
        assert_eq!(resp, Response::Err(ProtoError::Escape));
    }

    #[test]
    fn create_write_read_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut handles = HandleTable::new();

        let resp = dispatch(tmp.path(), &mut handles, Request::Create { path: "a.txt".into(), mode: 0o644 });
        let handle = match resp {
            Response::Created { handle, .. } => handle,
            other => panic!("expected Created, got {other:?}"),
        };

        let resp = dispatch(
            tmp.path(),
            &mut handles,
            Request::Write { handle, offset: 0, data: b"hello".to_vec() },
        );
        assert_eq!(resp, Response::Written(5));

        let resp = dispatch(tmp.path(), &mut handles, Request::Read { handle, offset: 0, len: 100 });
        assert_eq!(resp, Response::Data(b"hello".to_vec()));

        let resp = dispatch(tmp.path(), &mut handles, Request::Release(handle));
        assert_eq!(resp, Response::Ok);
        assert!(handles.is_empty());
    }

    #[test]
    fn readdir_lists_files_and_directories() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"x").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        let mut handles = HandleTable::new();
        let resp = dispatch(tmp.path(), &mut handles, Request::ReadDir(".".into()));
        let mut entries = match resp {
            Response::Entries(e) => e,
            other => panic!("expected Entries, got {other:?}"),
        };
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "a.txt");
        assert_eq!(entries[0].kind, FileKind::File);
        assert_eq!(entries[1].name, "sub");
        assert_eq!(entries[1].kind, FileKind::Dir);
    }

    #[test]
    fn read_of_unknown_handle_is_invalid_handle() {
        let handles = HandleTable::new();
        let resp = read(&handles, 42, 0, 10);
        assert_eq!(resp, Response::Err(ProtoError::InvalidHandle));
    }
}
