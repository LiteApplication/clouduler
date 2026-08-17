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

    /// A node whose docker is down has to keep voting, so the unit must not
    /// make docker a hard dependency.
    #[test]
    fn docker_is_wanted_but_not_required() {
        let text = unit().render();
        assert!(text.contains("Wants=network-online.target docker.service"));
        assert!(!text.contains("Requires="));
    }
}
