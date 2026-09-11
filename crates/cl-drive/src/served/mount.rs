//! Mount lifecycle for served drives this node uses but does not serve.
//!
//! Mounts a real NFSv4 filesystem directly via `mount(2)` — no `mount.nfs`
//! helper needed. Unlike NFSv3, NFSv4 has no separate MOUNT-protocol round
//! trip to fetch a root filehandle: the kernel client negotiates it itself,
//! in-band, from the well-known root, so a plain text options string handed
//! straight to `mount(2)` is enough to bring a mount up with no external
//! process involved.

use std::collections::{BTreeSet, HashMap};
use std::ffi::CString;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use tracing::warn;

use crate::error::{DriveError, Result};

/// How long the kernel NFS client waits for a reply before it counts as a
/// timeout, in deciseconds (so 50 = 5s).
const TIMEO_DECISECONDS: u32 = 50;
/// How many times the client retries a timed-out request before giving up
/// and returning an error to the caller. Combined with `soft` below (as
/// opposed to the kernel default of `hard`, which retries forever and never
/// surfaces an error), this is what makes a dead server detected rather than
/// hung on indefinitely.
const RETRANS: u32 = 3;

/// One served drive (or one volume of one) this node mounts, resolved to a
/// concrete server address by the caller (the daemon knows how to turn
/// `served_by` plus the cluster config into a `SocketAddr`; this crate does
/// not).
#[derive(Debug, Clone)]
pub struct MountSpec {
    pub drive: String,
    /// `Some` mounts just that one volume's subtree (the export still covers
    /// the whole drive root; only the client mounts a subpath of it), so
    /// several independent mounts of the same drive can coexist. `None`
    /// mounts the drive's whole root.
    pub volume: Option<String>,
    pub mountpoint: PathBuf,
    pub server: SocketAddr,
    /// The drive's real directory on `server`, as that node declared it
    /// (`Node.drives`). Needed because an NFSv4 mount source is a path in
    /// the server's *real* filesystem, not one relative to the export: with
    /// no `fsid=0` root export declared, the kernel's pseudo-filesystem
    /// mirrors the server's real absolute path down to each export rather
    /// than starting a fresh namespace at it, so mounting a volume means
    /// giving the server's real path to it, not just `/<volume>`.
    pub remote_root: PathBuf,
}

impl MountSpec {
    /// Unique key identifying this mount, used as the `MountManager` table
    /// key: a bare drive name for a whole-drive mount, `drive/volume` for a
    /// per-volume one, so two volumes of the same drive don't collide.
    pub fn key(&self) -> String {
        match &self.volume {
            Some(volume) => format!("{}/{volume}", self.drive),
            None => self.drive.clone(),
        }
    }

    /// The path portion of the NFS source spec (`server:/path`): the
    /// server's real path to the drive root, or to the volume subdirectory
    /// within it.
    fn export_path(&self) -> String {
        match &self.volume {
            Some(volume) => self.remote_root.join(volume).display().to_string(),
            None => self.remote_root.display().to_string(),
        }
    }
}

/// Whether this process can call `mount(2)` at all. Unlike FUSE, an NFS
/// mount has no `allow_other`-style opt-in for a non-root user to make it
/// readable by other uids — `mount(2)` itself needs `CAP_SYS_ADMIN`, which in
/// practice means root.
pub fn mount_permission() -> Result<()> {
    // Safety: geteuid takes no arguments and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        Ok(())
    } else {
        Err(DriveError::Config(
            "mounting a served drive over NFS needs root (CAP_SYS_ADMIN)".into(),
        ))
    }
}

/// Whether this host's kernel can mount NFSv4 at all: the client module is
/// either built in or already loaded.
///
/// This daemon calls `mount(2)` directly rather than going through the
/// `mount.nfs4` helper, which on a normal system is what makes the kernel
/// autoload the module on first use (`request_module()`). Bypassing that
/// helper means bypassing the autoload too — on a host where `nfsv4` isn't
/// already loaded, the very first mount just fails outright instead of
/// pulling the module in on demand, so this needs to be checked and loaded
/// (`modprobe nfsv4`) up front rather than discovered as a mount error.
pub fn client_available() -> Result<()> {
    client_available_from("/proc/filesystems", "/proc/modules")
}

fn client_available_from(filesystems_path: &str, modules_path: &str) -> Result<()> {
    let registered = std::fs::read_to_string(filesystems_path)
        .map(|s| {
            s.lines()
                .any(|line| line.split_whitespace().last() == Some("nfs4"))
        })
        .unwrap_or(false);
    let loaded = std::fs::read_to_string(modules_path)
        .map(|s| {
            s.lines()
                .any(|line| line.split_whitespace().next() == Some("nfsv4"))
        })
        .unwrap_or(false);
    if registered || loaded {
        return Ok(());
    }
    Err(DriveError::Config(
        "the kernel's NFSv4 client module (nfsv4) is not loaded — run `modprobe nfsv4` before this \
         node can mount a served drive"
            .into(),
    ))
}

fn cstring(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| DriveError::Config(format!("path has an embedded NUL: {s:?}")))
}

fn mount_options(server_ip: std::net::IpAddr, port: u16) -> String {
    // `noatime` is deliberately not here: it's a VFS-level mount flag
    // (`MS_NOATIME`), not an NFS filesystem-specific option, and the kernel
    // rejects it with EINVAL if it shows up in this text blob — it belongs
    // in `do_mount`'s `flags` argument instead.
    format!(
        "vers=4.2,proto=tcp,port={port},addr={server_ip},soft,timeo={TIMEO_DECISECONDS},\
         retrans={RETRANS},nolock"
    )
}

fn do_mount(spec: &MountSpec) -> Result<()> {
    std::fs::create_dir_all(&spec.mountpoint).map_err(|source| DriveError::Io {
        path: spec.mountpoint.clone(),
        source,
    })?;

    let source = format!("{}:{}", spec.server.ip(), spec.export_path());
    let data = mount_options(spec.server.ip(), spec.server.port());

    let c_source = cstring(&source)?;
    let c_target = cstring(&spec.mountpoint.to_string_lossy())?;
    let c_fstype = cstring("nfs4")?;
    let c_data = cstring(&data)?;

    // Safety: every pointer is a valid NUL-terminated `CString` kept alive
    // for the duration of this call. `MS_NOATIME` is the one VFS-level flag
    // this mount wants; everything NFS-specific goes through the text
    // `data` string instead (and must — the kernel's NFS option parser
    // rejects a VFS-level option like `noatime` showing up there).
    let rc = unsafe {
        libc::mount(
            c_source.as_ptr(),
            c_target.as_ptr(),
            c_fstype.as_ptr(),
            libc::MS_NOATIME,
            c_data.as_ptr().cast::<libc::c_void>(),
        )
    };
    if rc != 0 {
        return Err(DriveError::Io {
            path: spec.mountpoint.clone(),
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

/// Lazily unmount whatever is (or claims to be) at `path`, ignoring every
/// failure: it's a best-effort cleanup of a stale mount this process never
/// owned (a previous daemon crashed or was killed without unmounting), so a
/// missing mountpoint, a non-mount directory, or a permission error are all
/// fine to swallow and let the caller's own error handling take over.
fn force_unmount(path: &Path) {
    let Some(cpath) = path.to_str().and_then(|s| CString::new(s).ok()) else {
        return;
    };
    // Safety: `cpath` is a valid NUL-terminated C string for the lifetime of
    // this call; `umount2` does not retain it past returning.
    unsafe {
        libc::umount2(cpath.as_ptr(), libc::MNT_DETACH);
    }
}

/// A clean unmount; fails if the mountpoint is busy.
fn unmount_clean(path: &Path) -> Result<()> {
    let cpath = cstring(&path.to_string_lossy())?;
    // Safety: `cpath` is valid for the duration of this call.
    let rc = unsafe { libc::umount2(cpath.as_ptr(), 0) };
    if rc != 0 {
        return Err(DriveError::Io {
            path: path.to_path_buf(),
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

/// Whether `path` currently appears as a mounted filesystem, read from
/// `/proc/self/mountinfo` rather than trusted from this process's own
/// bookkeeping — catches a mount the kernel dropped on its own (e.g. after
/// repeated `NFS4ERR_STALE`) that an in-memory table alone wouldn't notice.
fn is_mount_point(path: &Path) -> bool {
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let target = path.to_string_lossy();
    info.lines()
        .any(|line| line.split_whitespace().nth(4) == Some(target.as_ref()))
}

/// A bounded `stat()` on `path`, run off the async runtime since it's a
/// blocking syscall that a wedged (but still `soft`-mounted) filesystem
/// could otherwise stall on. Times out rather than hanging, so a mount that
/// is present but unresponsive is still caught as unhealthy.
async fn stat_within(path: &Path, timeout: Duration) -> bool {
    let path = path.to_path_buf();
    let probe = tokio::task::spawn_blocking(move || std::fs::metadata(&path).is_ok());
    matches!(tokio::time::timeout(timeout, probe).await, Ok(Ok(true)))
}

struct ActiveMount {
    spec: MountSpec,
}

/// Mounts and unmounts every served drive this node uses. One instance per
/// daemon; every mount it owns comes down together in `unmount_all`.
#[derive(Default)]
pub struct MountManager {
    mounts: Mutex<HashMap<String, ActiveMount>>,
}

impl MountManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mount every spec. Stops and unwinds on the first failure, so the
    /// daemon never starts up half mounted.
    pub fn mount_all(&self, specs: Vec<MountSpec>) -> Result<()> {
        let mut mounted = Vec::new();
        for spec in specs {
            let key = spec.key();
            match self.mount_one(spec) {
                Ok(()) => mounted.push(key),
                Err(e) => {
                    for key in mounted {
                        let _ = self.unmount(&key);
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    pub fn mount_one(&self, spec: MountSpec) -> Result<()> {
        mount_permission()?;
        client_available()?;
        if is_mount_point(&spec.mountpoint) {
            // A stale mount from a previous process (daemon restart or
            // crash without a clean unmount) is squatting on this
            // mountpoint. Force it off before laying a fresh one down.
            force_unmount(&spec.mountpoint);
        }
        do_mount(&spec)?;
        let key = spec.key();
        self.mounts
            .lock()
            .expect("mount table lock")
            .insert(key, ActiveMount { spec });
        Ok(())
    }

    /// Unmount one key (see `MountSpec::key`). Not currently mounted is a
    /// no-op, not an error, so shutdown and convergence can call this
    /// unconditionally.
    pub fn unmount(&self, key: &str) -> Result<()> {
        let active = self.mounts.lock().expect("mount table lock").remove(key);
        if let Some(active) = active {
            unmount_clean(&active.spec.mountpoint)?;
        }
        Ok(())
    }

    /// Unmount everything, for a clean daemon shutdown. Keeps going after a
    /// failure so one stuck mount doesn't block the others from coming
    /// down; returns the first error seen, if any.
    pub fn unmount_all(&self) -> Result<()> {
        let keys: Vec<String> = self
            .mounts
            .lock()
            .expect("mount table lock")
            .keys()
            .cloned()
            .collect();
        let mut first_err = None;
        for key in keys {
            if let Err(e) = self.unmount(&key) {
                warn!(%key, error = %e, "failed to unmount served drive");
                first_err.get_or_insert(e);
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    /// Every key currently mounted (see `MountSpec::key`), for a caller that
    /// needs to converge the mount set against what's actually needed.
    pub fn mounted_keys(&self) -> BTreeSet<String> {
        self.mounts
            .lock()
            .expect("mount table lock")
            .keys()
            .cloned()
            .collect()
    }

    pub fn is_mounted(&self, key: &str) -> bool {
        self.mounts
            .lock()
            .expect("mount table lock")
            .contains_key(key)
    }

    /// Fail a mount over to a new active server. Unlike the old FUSE client
    /// (whose server address was a shared `Arc<Mutex<_>>` read on the next
    /// RPC), a kernel NFS mount bakes its server address (and its real
    /// remote path — see `MountSpec::remote_root`) in at `mount(2)` time —
    /// there is no live retarget, so this unmounts and remounts against
    /// `new_spec` instead. `new_spec` carries the failed-over server's own
    /// address and drive root, which can differ node to node. A no-op
    /// (`Ok(false)`) if `key` isn't mounted, or is already pointed at the
    /// same server and root.
    pub fn retarget(&self, key: &str, new_spec: MountSpec) -> Result<bool> {
        {
            let mounts = self.mounts.lock().expect("mount table lock");
            match mounts.get(key) {
                Some(active)
                    if active.spec.server == new_spec.server
                        && active.spec.remote_root == new_spec.remote_root =>
                {
                    return Ok(false);
                }
                Some(_) => {}
                None => return Ok(false),
            }
        }
        self.unmount(key)?;
        self.mount_one(new_spec)?;
        Ok(true)
    }

    /// Confirm every mount this manager owns is actually healthy — still
    /// present in `/proc/self/mountinfo`, and a bounded `stat()` on it
    /// doesn't time out — and force-remount any that aren't. This is the
    /// direct replacement for the old FUSE `remount_dead`: a `soft` NFS
    /// mount never dies the way a FUSE session visibly could, it just
    /// serves `EIO`/`ETIMEDOUT` per call forever once its server is gone,
    /// so recovery has to actively probe rather than wait for a session to
    /// end on its own.
    pub async fn check_health(&self) {
        let specs: Vec<MountSpec> = {
            let mounts = self.mounts.lock().expect("mount table lock");
            mounts.values().map(|m| m.spec.clone()).collect()
        };
        for spec in specs {
            let key = spec.key();
            let healthy = is_mount_point(&spec.mountpoint)
                && stat_within(&spec.mountpoint, Duration::from_secs(3)).await;
            if healthy {
                continue;
            }
            warn!(%key, "served drive mount looks unhealthy, remounting");
            force_unmount(&spec.mountpoint);
            self.mounts.lock().expect("mount table lock").remove(&key);
            if let Err(e) = self.mount_one(spec) {
                warn!(%key, error = %e, "failed to remount an unhealthy served drive volume");
            }
        }
    }

    /// Run forever, periodically health-checking and remounting any
    /// unhealthy mount. Intended to be spawned as a background task
    /// alongside the sync scheduler.
    pub async fn supervise(&self, interval: Duration) {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            self.check_health().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_available_when_nfs4_is_a_registered_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let filesystems = tmp.path().join("filesystems");
        let modules = tmp.path().join("modules");
        std::fs::write(&filesystems, "nodev\tsysfs\nnodev\tnfs4\n").unwrap();
        std::fs::write(&modules, "").unwrap();
        assert!(
            client_available_from(filesystems.to_str().unwrap(), modules.to_str().unwrap()).is_ok()
        );
    }

    #[test]
    fn client_available_when_nfsv4_module_is_loaded_but_not_yet_registered() {
        let tmp = tempfile::tempdir().unwrap();
        let filesystems = tmp.path().join("filesystems");
        let modules = tmp.path().join("modules");
        std::fs::write(&filesystems, "nodev\tsysfs\n").unwrap();
        std::fs::write(&modules, "nfsv4 000000 0 - Live 0x0000000000000000\n").unwrap();
        assert!(
            client_available_from(filesystems.to_str().unwrap(), modules.to_str().unwrap()).is_ok()
        );
    }

    #[test]
    fn client_unavailable_when_neither_shows_nfs4() {
        let tmp = tempfile::tempdir().unwrap();
        let filesystems = tmp.path().join("filesystems");
        let modules = tmp.path().join("modules");
        std::fs::write(&filesystems, "nodev\tsysfs\n").unwrap();
        std::fs::write(&modules, "ext4 000000 0 - Live 0x0000000000000000\n").unwrap();
        let err = client_available_from(filesystems.to_str().unwrap(), modules.to_str().unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("nfsv4"));
    }

    #[test]
    fn is_mounted_is_false_for_an_unknown_drive() {
        let manager = MountManager::new();
        assert!(!manager.is_mounted("fast"));
        assert!(manager.unmount_all().is_ok());
    }

    fn test_spec() -> MountSpec {
        MountSpec {
            drive: "fast".into(),
            volume: None,
            mountpoint: PathBuf::new(),
            server: "127.0.0.1:2049".parse().unwrap(),
            remote_root: PathBuf::from("/srv/clouduler/fast"),
        }
    }

    #[test]
    fn retarget_is_a_no_op_for_an_unmounted_drive() {
        let manager = MountManager::new();
        assert!(!manager.retarget("fast", test_spec()).unwrap());
    }

    #[test]
    fn mount_spec_key_distinguishes_whole_drive_from_a_volume_of_it() {
        let base = test_spec();
        assert_eq!(base.key(), "fast");
        let vol = MountSpec {
            volume: Some("data".into()),
            ..base
        };
        assert_eq!(vol.key(), "fast/data");
    }

    #[test]
    fn export_path_is_the_remote_root_or_a_volume_subpath_of_it() {
        let base = test_spec();
        assert_eq!(base.export_path(), "/srv/clouduler/fast");
        let vol = MountSpec {
            volume: Some("data".into()),
            ..base
        };
        assert_eq!(vol.export_path(), "/srv/clouduler/fast/data");
    }

    #[test]
    fn is_mount_point_is_false_for_a_plain_directory() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_mount_point(tmp.path()));
    }
}
