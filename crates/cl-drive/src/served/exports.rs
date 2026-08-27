//! Server side of a served drive under NFSv4: managing kernel `nfsd`
//! exports for the drive's real directory via `exportfs`, instead of
//! answering a bespoke protocol ourselves.
//!
//! `nfsd`/`rpc.mountd` are treated the way `dockerd` is treated elsewhere in
//! this daemon: an external system service this process depends on and
//! checks for, not one it spawns or supervises itself. What this module
//! owns is only the export table — which directories are visible over NFS
//! right now — kept in step with this node's `served_by` membership.

use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Command;

use crate::error::{DriveError, Result};

/// The port `nfsd` is expected to listen on. Fixed rather than derived per
/// drive: a kernel `nfsd` is one process per host serving every export,
/// distinguished by export path rather than by listening port, so unlike the
/// old bespoke protocol there is exactly one NFS port to agree on cluster
/// wide.
pub const NFS_PORT: u16 = 2049;

/// A stable numeric fsid for a drive's export, derived from its name so it
/// survives daemon restarts. A freshly random fsid on every restart would
/// invalidate every client's cached filehandles the moment this process
/// restarted; this only needs to be stable and unique per drive on this
/// host, not globally unique.
fn fsid_of(drive: &str) -> u32 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    drive.hash(&mut hasher);
    (hasher.finish() % 1_000_000) as u32 + 1
}

/// Ensure `root` is exported over NFSv4 for `drive`, restricted to
/// `client_subnet` (normally the cluster's internal-network subnet, the
/// same trust boundary the old served-drive protocol used: trusted,
/// unencrypted, internal-network-only). Idempotent — re-running with the
/// same arguments is a no-op as far as `exportfs` is concerned.
pub fn ensure_export(drive: &str, root: &Path, client_subnet: &str) -> Result<()> {
    let fsid = fsid_of(drive);
    let spec = format!("{client_subnet}:{}", root.display());
    let options = format!("rw,no_root_squash,no_subtree_check,fsid={fsid}");
    run_exportfs(&["-o", &options, &spec])
}

/// Remove the export for `root`, e.g. because this node stopped being a
/// `served_by` candidate for `drive`, or the drive itself was removed. Not
/// finding it exported in the first place is fine, not an error.
pub fn remove_export(root: &Path, client_subnet: &str) -> Result<()> {
    let spec = format!("{client_subnet}:{}", root.display());
    match run_exportfs(&["-u", &spec]) {
        Ok(()) => Ok(()),
        Err(DriveError::Transfer { message }) if message.to_lowercase().contains("not exported") => Ok(()),
        Err(e) => Err(e),
    }
}

fn run_exportfs(args: &[&str]) -> Result<()> {
    let output = Command::new("exportfs")
        .args(args)
        .output()
        .map_err(|source| DriveError::Io { path: Path::new("exportfs").to_path_buf(), source })?;
    if !output.status.success() {
        return Err(DriveError::Transfer {
            message: format!("exportfs {args:?} failed: {}", String::from_utf8_lossy(&output.stderr)),
        });
    }
    Ok(())
}

/// Whether this host looks ready to serve an NFSv4 export: kernel NFS
/// server support and `exportfs` are present, and a trivial `exportfs -v`
/// call (list current exports) succeeds. Checked at daemon startup and by
/// `clouduler init`, non-fatal either way — a node that can't serve simply
/// never becomes an active server for anything.
pub fn check_available() -> Result<()> {
    if !Path::new("/proc/fs/nfsd").exists() && !kernel_module_present("nfsd") {
        return Err(DriveError::Config("kernel NFS server support (nfsd) is not available".into()));
    }
    run_exportfs(&["-v"])
}

fn kernel_module_present(name: &str) -> bool {
    std::fs::read_to_string("/proc/modules")
        .map(|s| s.lines().any(|l| l.split_whitespace().next() == Some(name)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fsid_is_deterministic_and_differs_per_drive() {
        assert_eq!(fsid_of("fast"), fsid_of("fast"));
        assert_ne!(fsid_of("fast"), fsid_of("bulk"));
        assert!(fsid_of("fast") > 0);
    }
}
