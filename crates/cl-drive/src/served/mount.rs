//! Mount lifecycle for served drives this node uses but does not serve.
//!
//! A FUSE mount needs either the mounting process to be root, or
//! `user_allow_other` set in `/etc/fuse.conf` (the kernel refuses
//! `allow_other` from a non-root user otherwise, since it would let a
//! mount owned by one user be read by every other user on the box). The
//! daemon needs `allow_other` regardless of which user runs it: whatever
//! uid the containers bind-mounting a served drive run as has to be able
//! to read it, and that uid is almost never the mounting process's own.
//! Detecting the missing permission up front and naming it produces a
//! useful error instead of an opaque `EPERM` from deep inside `fuser`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use tracing::warn;

use crate::error::{DriveError, Result};
use crate::served::client::{ClientConfig, ClientFs};

const FUSE_CONF: &str = "/etc/fuse.conf";

/// One served drive this node mounts, resolved to a concrete server
/// address by the caller (the daemon knows how to turn `served_by` plus
/// the cluster config into a `SocketAddr`; this crate does not).
#[derive(Debug, Clone)]
pub struct MountSpec {
    pub drive: String,
    pub mountpoint: PathBuf,
    pub server: SocketAddr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountPermission {
    Root,
    /// Non-root, but `/etc/fuse.conf` allows it.
    AllowOther,
}

/// Whether this process can mount with `allow_other`, and if not, why.
pub fn mount_permission() -> Result<MountPermission> {
    // Safety: geteuid takes no arguments and cannot fail.
    mount_permission_from(unsafe { libc::geteuid() }, Path::new(FUSE_CONF))
}

fn mount_permission_from(euid: u32, fuse_conf: &Path) -> Result<MountPermission> {
    if euid == 0 {
        return Ok(MountPermission::Root);
    }
    let conf = std::fs::read_to_string(fuse_conf).unwrap_or_default();
    let allowed = conf.lines().map(str::trim).filter(|l| !l.starts_with('#')).any(|l| l == "user_allow_other");
    if allowed {
        Ok(MountPermission::AllowOther)
    } else {
        Err(DriveError::Config(format!(
            "mounting a served drive needs either root or `user_allow_other` set in {}",
            fuse_conf.display()
        )))
    }
}

struct ActiveMount {
    spec: MountSpec,
    session: fuser::BackgroundSession,
}

/// Mounts and unmounts every served drive this node uses. One instance per
/// daemon; every mount it owns comes down together in `unmount_all`.
pub struct MountManager {
    rt: tokio::runtime::Handle,
    config: ClientConfig,
    mounts: Mutex<HashMap<String, ActiveMount>>,
}

impl MountManager {
    /// `rt` must belong to a multi-threaded runtime; see `ClientFs::new`.
    pub fn new(rt: tokio::runtime::Handle) -> Self {
        Self::with_config(rt, ClientConfig::default())
    }

    pub fn with_config(rt: tokio::runtime::Handle, config: ClientConfig) -> Self {
        Self { rt, config, mounts: Mutex::new(HashMap::new()) }
    }

    /// Mount every spec. Stops and unwinds on the first failure, so the
    /// daemon never starts up half mounted.
    pub fn mount_all(&self, specs: Vec<MountSpec>) -> Result<()> {
        let mut mounted = Vec::new();
        for spec in specs {
            let drive = spec.drive.clone();
            match self.mount_one(spec) {
                Ok(()) => mounted.push(drive),
                Err(e) => {
                    for drive in mounted {
                        let _ = self.unmount(&drive);
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    pub fn mount_one(&self, spec: MountSpec) -> Result<()> {
        mount_permission()?;
        std::fs::create_dir_all(&spec.mountpoint)
            .map_err(|source| DriveError::Io { path: spec.mountpoint.clone(), source })?;

        let fs = ClientFs::with_config(spec.server, self.rt.clone(), self.config);
        let options = vec![
            fuser::MountOption::FSName(format!("clouduler-{}", spec.drive)),
            fuser::MountOption::Subtype("clouduler".into()),
            fuser::MountOption::NoAtime,
        ];
        // `Config` is `#[non_exhaustive]`, so build from `default()` and set
        // fields rather than a struct literal. `allow_other` needs both the
        // mount option string and this ACL flag; the ACL is what makes
        // fuser actually pass `allow_other` through instead of silently
        // dropping it for a non-root user without `user_allow_other`.
        let mut session_config = fuser::Config::default();
        session_config.mount_options = options;
        // Containers on this node almost never run as the mounting
        // process's own uid, so every served drive needs `allow_other`.
        // The `mount_permission` check above already established that's
        // possible here, either because we're root or because the
        // operator opted in via `user_allow_other`.
        session_config.acl = fuser::SessionACL::All;

        let session = fuser::spawn_mount(fs, &spec.mountpoint, &session_config)
            .map_err(|source| DriveError::Io { path: spec.mountpoint.clone(), source })?;

        self.mounts.lock().expect("mount table lock").insert(spec.drive.clone(), ActiveMount { spec, session });
        Ok(())
    }

    /// Unmount one drive. A drive not currently mounted is a no-op, not an
    /// error, so shutdown can call this unconditionally.
    pub fn unmount(&self, drive: &str) -> Result<()> {
        let active = self.mounts.lock().expect("mount table lock").remove(drive);
        if let Some(active) = active {
            active
                .session
                .umount_and_join()
                .map_err(|source| DriveError::Io { path: active.spec.mountpoint, source })?;
        }
        Ok(())
    }

    /// Unmount everything, for a clean daemon shutdown. Keeps going after a
    /// failure so one stuck mount doesn't block the others from coming
    /// down; returns the first error seen, if any.
    pub fn unmount_all(&self) -> Result<()> {
        let drives: Vec<String> = self.mounts.lock().expect("mount table lock").keys().cloned().collect();
        let mut first_err = None;
        for drive in drives {
            if let Err(e) = self.unmount(&drive) {
                warn!(%drive, error = %e, "failed to unmount served drive");
                first_err.get_or_insert(e);
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    pub fn is_mounted(&self, drive: &str) -> bool {
        self.mounts.lock().expect("mount table lock").contains_key(drive)
    }

    /// Remount any drive whose FUSE session has exited on its own (a fatal
    /// session error, not the ordinary case of the server being briefly
    /// unreachable: `ClientFs` reports `EIO` per request and reconnects on
    /// the next one without the session ever dying). Called periodically
    /// by `supervise`; the serving node coming back does not, by itself,
    /// require anything here, since the mount was never taken down.
    pub fn remount_dead(&self) -> Result<()> {
        let dead: Vec<MountSpec> = {
            let mounts = self.mounts.lock().expect("mount table lock");
            mounts.values().filter(|m| m.session.guard.is_finished()).map(|m| m.spec.clone()).collect()
        };
        for spec in dead {
            let drive = spec.drive.clone();
            self.mounts.lock().expect("mount table lock").remove(&drive);
            warn!(%drive, "served drive mount session exited unexpectedly, remounting");
            self.mount_one(spec)?;
        }
        Ok(())
    }

    /// Run forever, periodically remounting any dead session. Intended to
    /// be spawned as a background task alongside the sync scheduler.
    pub async fn supervise(&self, interval: Duration) {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            if let Err(e) = self.remount_dead() {
                warn!(error = %e, "failed to remount a served drive");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_can_always_mount() {
        let tmp = tempfile::tempdir().unwrap();
        let conf = tmp.path().join("fuse.conf");
        assert_eq!(mount_permission_from(0, &conf).unwrap(), MountPermission::Root);
    }

    #[test]
    fn non_root_needs_user_allow_other_in_fuse_conf() {
        let tmp = tempfile::tempdir().unwrap();
        let conf = tmp.path().join("fuse.conf");
        let err = mount_permission_from(1000, &conf).unwrap_err();
        assert!(matches!(err, DriveError::Config(_)));
        assert!(err.to_string().contains("user_allow_other"));
    }

    #[test]
    fn non_root_is_allowed_once_the_conf_file_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let conf = tmp.path().join("fuse.conf");
        std::fs::write(&conf, "# comment\nuser_allow_other\n").unwrap();
        assert_eq!(mount_permission_from(1000, &conf).unwrap(), MountPermission::AllowOther);
    }

    #[test]
    fn a_commented_out_line_does_not_count() {
        let tmp = tempfile::tempdir().unwrap();
        let conf = tmp.path().join("fuse.conf");
        std::fs::write(&conf, "# user_allow_other\n").unwrap();
        assert!(mount_permission_from(1000, &conf).is_err());
    }

    #[test]
    fn is_mounted_is_false_for_an_unknown_drive() {
        let rt = tokio::runtime::Handle::try_current()
            .unwrap_or_else(|_| tokio::runtime::Runtime::new().unwrap().handle().clone());
        let manager = MountManager::new(rt);
        assert!(!manager.is_mounted("fast"));
        assert!(manager.unmount_all().is_ok());
    }
}
