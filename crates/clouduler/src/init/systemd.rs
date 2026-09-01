//! The systemd unit init installs for the daemon.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

pub const UNIT_NAME: &str = "clouduler.service";
pub const UNIT_DIR: &str = "/etc/systemd/system";

/// What the unit needs to know about this node.
pub struct Unit {
    pub binary: PathBuf,
    pub config: PathBuf,
    pub node: String,
}

impl Unit {
    /// `Wants` rather than `Requires` on docker: a manager with no docker still
    /// votes, holds the replicated state and serves the ui, so the daemon must
    /// come up on a node where docker is broken. It is the node people go to to
    /// find out why.
    ///
    /// Still runs as root (`User=` is deliberately left unset): docker and the
    /// overlay networking it manages (creating VXLAN interfaces, bridges, veth
    /// pairs via rtnetlink) need real root, not just membership of a group.
    /// `Group=` only sets root's *primary* group here, which costs nothing —
    /// root bypasses the unix permission model outright — but keeps the
    /// process's own group identity consistent with the `clouduler` group the
    /// daemon chowns its files to on startup, in case anything ever cares.
    pub fn render(&self) -> String {
        format!(
            "[Unit]\n\
             Description=Clouduler node daemon\n\
             Documentation=https://github.com/LiteApplication/clouduler\n\
             Wants=network-online.target docker.service\n\
             After=network-online.target docker.service\n\
             \n\
             [Service]\n\
             Type=simple\n\
             Group=clouduler\n\
             ExecStart={binary} --config {config} daemon --node {node}\n\
             Restart=always\n\
             RestartSec=5\n\
             TimeoutStopSec=30\n\
             LimitNOFILE=65536\n\
             \n\
             [Install]\n\
             WantedBy=multi-user.target\n",
            binary = self.binary.display(),
            config = self.config.display(),
            node = self.node,
        )
    }
}

pub fn unit_path() -> PathBuf {
    Path::new(UNIT_DIR).join(UNIT_NAME)
}

pub fn is_available() -> bool {
    Path::new("/run/systemd/system").is_dir()
}

/// Copy this executable to where the unit will call it from.
///
/// init is normally run straight from a downloaded binary sitting in a home
/// directory or /tmp, and a unit pointing there breaks the moment that file is
/// cleaned up or the user is removed. Copying first makes the installed service
/// depend only on paths root controls.
///
/// Shared with the self-update worker via `cl_update::install`, which does
/// the actual temp-file-and-rename swap: a running daemon cannot have its own
/// executable overwritten in place (ETXTBSY), but can have the directory
/// entry swapped underneath it.
pub fn install_binary(source: &Path, destination: &Path) -> Result<()> {
    cl_update::install::install_binary(source, destination).with_context(|| {
        format!(
            "installing {} to {}",
            source.display(),
            destination.display()
        )
    })
}

/// Write the unit and reload systemd so it is visible.
pub fn install(unit: &Unit) -> Result<PathBuf> {
    let path = unit_path();
    let dir = path.parent().expect("unit path has a directory");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(&path, unit.render()).with_context(|| format!("writing {}", path.display()))?;
    systemctl(&["daemon-reload"])?;
    Ok(path)
}

/// Enable at boot and start now.
pub fn enable_and_start() -> Result<()> {
    systemctl(&["enable", "--now", UNIT_NAME])
}

fn systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .args(args)
        .output()
        .with_context(|| format!("running systemctl {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "systemctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn unit() -> Unit {
        Unit {
            binary: PathBuf::from("/usr/local/bin/clouduler"),
            config: PathBuf::from("/etc/clouduler/cluster.yaml"),
            node: "n1".into(),
        }
    }

    #[test]
    fn the_unit_starts_the_daemon_with_this_nodes_name_and_config() {
        let text = unit().render();
        assert!(text.contains(
            "ExecStart=/usr/local/bin/clouduler --config /etc/clouduler/cluster.yaml daemon --node n1"
        ));
    }

    #[test]
    fn the_unit_restarts_the_daemon_and_installs_for_boot() {
        let text = unit().render();
        assert!(text.contains("Restart=always"));
        assert!(text.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn installing_the_binary_copies_it_and_makes_it_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("downloaded");
        std::fs::write(&source, b"#!/bin/true\n").expect("write source");
        let destination = dir.path().join("bin").join("clouduler");

        install_binary(&source, &destination).expect("install");
        assert_eq!(std::fs::read(&destination).expect("read"), b"#!/bin/true\n");
        let mode = std::fs::metadata(&destination).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    /// Re-running init against the installed binary must not truncate it by
    /// copying a file onto itself.
    #[test]
    fn installing_a_binary_over_itself_is_a_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("clouduler");
        std::fs::write(&path, b"contents").expect("write");

        install_binary(&path, &path).expect("install");
        assert_eq!(std::fs::read(&path).expect("read"), b"contents");
    }

    /// A node whose docker is down has to keep voting, so the unit must not
    /// make docker a hard dependency.
    #[test]
    fn docker_is_wanted_but_not_required() {
        let text = unit().render();
        assert!(text.contains("Wants=network-online.target docker.service"));
        assert!(!text.contains("Requires="));
    }

    /// Root still runs the daemon (docker/network operations need real root),
    /// but its group matches what startup chowns cluster files to.
    #[test]
    fn the_daemon_runs_with_the_clouduler_group_but_stays_root() {
        let text = unit().render();
        assert!(text.contains("Group=clouduler"));
        assert!(!text.contains("User="));
    }
}
