//! Server side of a served drive under NFSv4: managing kernel `nfsd` exports
//! via `exportfs`. `nfsd`/`rpc.mountd` are external system services (like
//! `dockerd`); this module owns only the export table, kept in step with this
//! node's `served_by` membership.

use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Command;

use crate::error::{DriveError, Result};

/// Fixed: kernel `nfsd` is one process per host, distinguished by export path.
pub const NFS_PORT: u16 = 2049;

/// Stable fsid derived from the drive name. A random one per restart would
/// invalidate clients' cached filehandles; it only needs to be unique per host.
fn fsid_of(drive: &str) -> u32 {
    let mut hasher = cl_config::stable_hasher();
    drive.hash(&mut hasher);
    (hasher.finish() % 1_000_000) as u32 + 1
}

/// Export `root` over NFSv4 to `client_subnet` (the internal-network subnet:
/// trusted, unencrypted). Idempotent.
pub fn ensure_export(drive: &str, root: &Path, client_subnet: &str) -> Result<()> {
    let fsid = fsid_of(drive);
    let spec = format!("{client_subnet}:{}", root.display());
    let options = format!("rw,no_root_squash,no_subtree_check,fsid={fsid}");
    run_exportfs(&["-o", &options, &spec])
}

/// Remove the export for `root`; already-not-exported is fine.
pub fn remove_export(root: &Path, client_subnet: &str) -> Result<()> {
    let spec = format!("{client_subnet}:{}", root.display());
    match run_exportfs(&["-u", &spec]) {
        Ok(()) => Ok(()),
        Err(DriveError::Transfer { message })
            if message.to_lowercase().contains("not exported") =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn run_exportfs(args: &[&str]) -> Result<()> {
    let output = Command::new("exportfs")
        .args(args)
        .output()
        .map_err(|source| DriveError::Io {
            path: Path::new("exportfs").to_path_buf(),
            source,
        })?;
    if !output.status.success() {
        return Err(DriveError::Transfer {
            message: format!(
                "exportfs {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        });
    }
    Ok(())
}

/// Whether this host can serve NFSv4: nfsd support and `exportfs` present, and
/// nfsd server threads actually running. `exportfs` accepts exports with no
/// threads, so without the last check a broken node looks fine until the first
/// mount gets connection refused. Non-fatal at startup and `clouduler init`.
pub fn check_available() -> Result<()> {
    if !Path::new("/proc/fs/nfsd").exists() && !kernel_module_present("nfsd") {
        return Err(DriveError::Config(
            "kernel NFS server support (nfsd) is not available".into(),
        ));
    }
    run_exportfs(&["-v"])?;
    if !server_running_from("/proc/fs/nfsd/pool_threads") {
        return Err(DriveError::Config(
            "nfsd is loaded but has no server threads running (start it with `rpc.nfsd <n>`, \
             e.g. via the nfs-kernel-server/nfs-utils service) — served drives will otherwise \
             export fine and then refuse every mount"
                .into(),
        ));
    }
    Ok(())
}

fn kernel_module_present(name: &str) -> bool {
    std::fs::read_to_string("/proc/modules")
        .map(|s| s.lines().any(|l| l.split_whitespace().next() == Some(name)))
        .unwrap_or(false)
}

/// `pool_threads` holds one count per pool; all `0` or unreadable (non-root, or
/// nfsd fs not mounted) means no server threads.
fn server_running_from(pool_threads_path: &str) -> bool {
    std::fs::read_to_string(pool_threads_path)
        .map(|s| {
            s.split_whitespace()
                .filter_map(|n| n.parse::<u32>().ok())
                .sum::<u32>()
                > 0
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Guards the move off `DefaultHasher` on the current toolchain. If a future
    // rustc changes `DefaultHasher`, this may start failing: that is expected,
    // delete it then and keep the pinned values.
    #[test]
    fn fsid_matches_default_hasher_today() {
        for drive in ["fast", "bulk", "veryfast", "a"] {
            let mut old = std::collections::hash_map::DefaultHasher::new();
            drive.hash(&mut old);
            assert_eq!(fsid_of(drive), (old.finish() % 1_000_000) as u32 + 1);
        }
    }

    #[test]
    fn fsid_values_are_pinned() {
        assert_eq!(fsid_of("fast"), 620140);
        assert_eq!(fsid_of("bulk"), 644159);
    }

    #[test]
    fn fsid_is_deterministic_and_differs_per_drive() {
        assert_eq!(fsid_of("fast"), fsid_of("fast"));
        assert_ne!(fsid_of("fast"), fsid_of("bulk"));
        assert!(fsid_of("fast") > 0);
    }

    #[test]
    fn server_running_is_false_when_every_thread_pool_is_empty() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "0 0\n").unwrap();
        assert!(!server_running_from(tmp.path().to_str().unwrap()));
    }

    #[test]
    fn server_running_is_true_once_any_thread_pool_has_threads() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "8\n").unwrap();
        assert!(server_running_from(tmp.path().to_str().unwrap()));
    }

    #[test]
    fn server_running_is_false_when_the_file_is_missing() {
        assert!(!server_running_from("/nonexistent/pool_threads"));
    }
}
