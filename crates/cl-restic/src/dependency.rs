//! Whether `restic` itself is available on this node.
//!
//! Checked once at daemon startup and again at `clouduler init`, advisory
//! only - exact mirror of `cl_net::vxlan::ethtool_available`/
//! `cl_drive::client_available`: a missing binary should surface as a clear
//! startup warning rather than only being discoverable the first time a
//! scheduled backup tries to run and silently never does.

pub fn restic_available() -> bool {
    std::process::Command::new("restic")
        .arg("version")
        .output()
        .is_ok_and(|output| output.status.success())
}
