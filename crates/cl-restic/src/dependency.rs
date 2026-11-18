//! Whether `restic` itself is available on this node.
//!
//! Checked at daemon startup and `clouduler init`, advisory only (mirrors
//! `cl_net::vxlan::ethtool_available`), so a missing binary is a clear warning
//! rather than scheduled backups silently never running.

pub fn restic_available() -> bool {
    std::process::Command::new("restic")
        .arg("version")
        .output()
        .is_ok_and(|output| output.status.success())
}
