//! Mount lifecycle for served drives this node uses but does not serve.
//!
//! Mounts real NFSv4 directly via `mount(2)`, no `mount.nfs` helper: NFSv4 has
//! no separate MOUNT round trip, so a text options string suffices.

use parking_lot::Mutex;
use std::collections::{BTreeSet, HashMap};
use std::ffi::CString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::warn;

use crate::error::{DriveError, Result};

/// Kernel NFS client reply timeout in deciseconds (50 = 5s).
const TIMEO_DECISECONDS: u32 = 50;
/// Retries of a timed-out request before erroring. With `soft` (the default
/// `hard` retries forever) this makes a dead server detected rather than hung on.
const RETRANS: u32 = 3;

/// One served drive (or volume of one) this node mounts, resolved to a server
/// address by the caller (the daemon turns `served_by` plus config into a
/// `SocketAddr`).
#[derive(Debug, Clone)]
pub struct MountSpec {
    pub drive: String,
    /// `Some` mounts just that volume's subtree (the export still covers the whole
    /// drive), so several mounts of one drive coexist. `None` mounts the root.
    pub volume: Option<String>,
    pub mountpoint: PathBuf,
    pub server: SocketAddr,
    /// The drive's real directory on `server` (`Node.drives`). An NFSv4 mount
    /// source is a path in the server's *real* filesystem: with no `fsid=0` root
    /// export, the pseudo-filesystem mirrors real absolute paths, so mounting a
    /// volume needs its real path, not `/<volume>`.
    pub remote_root: PathBuf,
}

impl MountSpec {
    /// Unique `MountManager` table key: drive name for a whole-drive mount,
    /// `drive/volume` for a per-volume one.
    pub fn key(&self) -> String {
        match &self.volume {
            Some(volume) => format!("{}/{volume}", self.drive),
            None => self.drive.clone(),
        }
    }

    /// The path of the NFS source spec (`server:/path`): the real path to the
    /// drive root or volume subdirectory.
    fn export_path(&self) -> String {
        match &self.volume {
            Some(volume) => self.remote_root.join(volume).display().to_string(),
            None => self.remote_root.display().to_string(),
        }
    }
}

/// Whether this process can call `mount(2)`: needs `CAP_SYS_ADMIN`, in practice root.
pub fn mount_permission() -> Result<()> {
    if nix::unistd::geteuid().is_root() {
        Ok(())
    } else {
        Err(DriveError::Config(
            "mounting a served drive over NFS needs root (CAP_SYS_ADMIN)".into(),
        ))
    }
}

/// Whether the kernel can mount NFSv4: client module built in or loaded.
///
/// Calling `mount(2)` directly bypasses `mount.nfs4`, which normally triggers
/// module autoload (`request_module()`), so a host without `nfsv4` loaded would
/// fail the first mount outright. Check and load (`modprobe nfsv4`) up front.
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
    // `noatime` is not here: it's a VFS flag (`MS_NOATIME`), and the kernel rejects
    // it with EINVAL in this text blob; it belongs in `do_mount`'s `flags`.
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

    // `MS_NOATIME` is the one VFS-level flag; NFS options go through the text
    // `data` string (the parser rejects VFS options like `noatime` there).
    nix::mount::mount(
        Some(c_source.as_c_str()),
        c_target.as_c_str(),
        Some(c_fstype.as_c_str()),
        nix::mount::MsFlags::MS_NOATIME,
        Some(c_data.as_c_str()),
    )
    .map_err(|errno| DriveError::Io {
        path: spec.mountpoint.clone(),
        source: errno.into(),
    })
}

/// Lazily unmount whatever is at `path`, ignoring every failure: best-effort
/// cleanup of a stale mount from a crashed daemon; the caller's own error
/// handling takes over.
fn force_unmount(path: &Path) {
    let _ = nix::mount::umount2(path, nix::mount::MntFlags::MNT_DETACH);
}

fn unmount_clean(path: &Path) -> Result<()> {
    nix::mount::umount2(path, nix::mount::MntFlags::empty()).map_err(|errno| DriveError::Io {
        path: path.to_path_buf(),
        source: errno.into(),
    })
}

/// Whether `path` is mounted per `/proc/self/mountinfo`, not this process's
/// bookkeeping: catches a mount the kernel dropped (e.g. repeated `NFS4ERR_STALE`).
fn is_mount_point(path: &Path) -> bool {
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let target = path.to_string_lossy();
    info.lines()
        .any(|line| line.split_whitespace().nth(4) == Some(target.as_ref()))
}

/// A bounded `stat()` on `path`, off the async runtime: a wedged `soft`-mounted
/// filesystem could stall it. Times out so a present-but-unresponsive mount is
/// caught as unhealthy.
async fn stat_within(path: &Path, timeout: Duration) -> bool {
    let path = path.to_path_buf();
    let probe = tokio::task::spawn_blocking(move || std::fs::metadata(&path).is_ok());
    matches!(tokio::time::timeout(timeout, probe).await, Ok(Ok(true)))
}

struct ActiveMount {
    spec: MountSpec,
}

/// Mounts and unmounts every served drive this node uses; one per daemon.
#[derive(Default)]
pub struct MountManager {
    mounts: Mutex<HashMap<String, ActiveMount>>,
}

impl MountManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mount every spec, unwinding on the first failure so the daemon never
    /// starts half mounted.
    pub fn mount_all(&self, specs: Vec<MountSpec>) -> Result<()> {
        let mut mounted = Vec::new();
        for spec in specs {
            let key = spec.key();
            match self.mount_one(spec) {
                Ok(()) => mounted.push(key),
                Err(e) => {
                    for key in mounted {
                        if let Err(e) = self.unmount(&key) {
                            tracing::warn!(error = %e, "could not unmount after a failed mount batch");
                        }
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
            // A stale mount from a previous process squats on this mountpoint; force it off.
            force_unmount(&spec.mountpoint);
        }
        do_mount(&spec)?;
        let key = spec.key();
        self.mounts.lock().insert(key, ActiveMount { spec });
        Ok(())
    }

    /// Unmount one key (see `MountSpec::key`); not mounted is a no-op.
    pub fn unmount(&self, key: &str) -> Result<()> {
        let active = self.mounts.lock().remove(key);
        if let Some(active) = active {
            unmount_clean(&active.spec.mountpoint)?;
        }
        Ok(())
    }

    /// Unmount everything for shutdown, continuing past failures; returns the first error.
    pub fn unmount_all(&self) -> Result<()> {
        let keys: Vec<String> = self.mounts.lock().keys().cloned().collect();
        let mut first_err = None;
        for key in keys {
            if let Err(e) = self.unmount(&key) {
                warn!(%key, error = %e, "failed to unmount served drive");
                first_err.get_or_insert(e);
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    /// Every key currently mounted, for converging against what is needed.
    pub fn mounted_keys(&self) -> BTreeSet<String> {
        self.mounts.lock().keys().cloned().collect()
    }

    pub fn is_mounted(&self, key: &str) -> bool {
        self.mounts.lock().contains_key(key)
    }

    /// Fail a mount over to a new active server. A kernel NFS mount bakes in its
    /// server address and real remote path (`MountSpec::remote_root`) at
    /// `mount(2)` time, so this unmounts and remounts against `new_spec` (which
    /// can differ node to node). `Ok(false)` if `key` isn't mounted or already
    /// points at the same server and root.
    pub fn retarget(&self, key: &str, new_spec: MountSpec) -> Result<bool> {
        {
            let mounts = self.mounts.lock();
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

    /// Check every mount is healthy (in `/proc/self/mountinfo` and a bounded
    /// `stat()` succeeds) and force-remount any that aren't. A `soft` NFS mount
    /// never visibly dies; it serves `EIO`/`ETIMEDOUT` forever once its server is
    /// gone, so recovery must probe.
    pub async fn check_health(&self) {
        let specs: Vec<MountSpec> = {
            let mounts = self.mounts.lock();
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
            self.mounts.lock().remove(&key);
            if let Err(e) = self.mount_one(spec) {
                warn!(%key, error = %e, "failed to remount an unhealthy served drive volume");
            }
        }
    }

    /// Run forever, health-checking and remounting; spawn as a background task.
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
