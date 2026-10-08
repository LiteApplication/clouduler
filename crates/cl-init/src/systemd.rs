//! The systemd unit init installs for the daemon.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

pub const UNIT_NAME: &str = "clouduler.service";
pub const SOCKET_NAME: &str = "clouduler.socket";
/// Exit status of a daemon that found it was removed from the cluster. The unit
/// must not restart it: it has wiped its key and raft state, so a restart would
/// only fail the same way every five seconds, forever. Same value as
/// `cl_daemon::daemon::REMOVED_EXIT_CODE` (`EX_CONFIG`).
pub const REMOVED_EXIT_CODE: u8 = 78;
pub const UNIT_DIR: &str = "/etc/systemd/system";

/// What the unit needs to know about this node.
pub struct Unit {
    pub binary: PathBuf,
    pub config: PathBuf,
    pub node: String,
    /// The reverse proxy's http and https ports, when it is enabled.
    pub proxy_ports: Option<(u16, u16)>,
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
             Type=notify\n\
             NotifyAccess=main\n\
             Group=clouduler\n\
             ExecStart={binary} --config {config} daemon --node {node}\n\
             ExecReload=/bin/kill -HUP $MAINPID\n\
             Restart=always\n\
             RestartSec=5\n\
             RestartPreventExitStatus={removed}\n\
             TimeoutStartSec=300\n\
             TimeoutStopSec=30\n\
             WatchdogSec=120\n\
             KillMode=mixed\n\
             SyslogIdentifier=clouduler\n\
             LimitNOFILE=65536\n\
             \n\
             [Install]\n\
             WantedBy=multi-user.target\n",
            binary = self.binary.display(),
            config = self.config.display(),
            node = self.node,
            removed = REMOVED_EXIT_CODE,
        )
    }
}

impl Unit {
    /// The socket unit that owns the proxy ports. systemd keeps them bound
    /// and queues connections while the daemon restarts, so a restart delays
    /// requests instead of refusing them. The daemon adopts the descriptors
    /// (`LISTEN_FDS`), and binds the ports itself when run without this.
    pub fn render_socket(&self) -> Option<String> {
        let (http, https) = self.proxy_ports?;
        Some(format!(
            "[Unit]\n\
             Description=Clouduler reverse proxy sockets\n\
             Documentation=https://github.com/LiteApplication/clouduler\n\
             \n\
             [Socket]\n\
             ListenStream=0.0.0.0:{http}\n\
             ListenStream=0.0.0.0:{https}\n\
             Backlog=1024\n\
             \n\
             [Install]\n\
             WantedBy=sockets.target\n"
        ))
    }
}

pub fn socket_path() -> PathBuf {
    Path::new(UNIT_DIR).join(SOCKET_NAME)
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
    match unit.render_socket() {
        Some(socket) => {
            let socket_path = socket_path();
            std::fs::write(&socket_path, socket)
                .with_context(|| format!("writing {}", socket_path.display()))?;
        }
        // Proxy turned off: a leftover socket would keep holding the ports.
        None => {
            let _ = std::fs::remove_file(socket_path());
        }
    }
    systemctl(&["daemon-reload"])?;
    Ok(path)
}

/// Enable at boot and start now.
pub fn enable_and_start(unit: &Unit) -> Result<()> {
    if unit.proxy_ports.is_some() {
        // The socket first, so the ports are held before the daemon is up.
        systemctl(&["enable", "--now", SOCKET_NAME])?;
    }
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
            proxy_ports: Some((80, 443)),
        }
    }

    #[test]
    fn the_proxy_ports_are_held_by_a_socket_unit() {
        let text = unit().render_socket().expect("proxy enabled");
        assert!(text.contains("ListenStream=0.0.0.0:80\n"));
        assert!(text.contains("ListenStream=0.0.0.0:443\n"));
        assert!(text.contains("WantedBy=sockets.target"));
    }

    #[test]
    fn no_proxy_means_no_socket_unit() {
        let mut no_proxy = unit();
        no_proxy.proxy_ports = None;
        assert!(no_proxy.render_socket().is_none());
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

    /// The daemon reports readiness itself, answers SIGHUP as a reload, pings
    /// a watchdog, and only its main process gets the stop signal so that
    /// restic/fuse children can finish before being killed.
    #[test]
    fn the_unit_speaks_the_service_manager_protocol() {
        let text = unit().render();
        assert!(text.contains("Type=notify\n"));
        assert!(text.contains("NotifyAccess=main"));
        assert!(text.contains("ExecReload=/bin/kill -HUP $MAINPID"));
        assert!(text.contains("WatchdogSec="));
        assert!(text.contains("KillMode=mixed"));
    }

    /// A removed node wipes its key; restarting it would loop forever.
    #[test]
    fn a_removed_node_is_not_restarted() {
        assert!(unit().render().contains("RestartPreventExitStatus=78"));
    }

    #[test]
    fn installing_the_binary_copies_it_and_makes_it_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("downloaded");
        std::fs::write(&source, b"#!/bin/true\n").expect("write source");
        let destination = dir.path().join("bin").join("clouduler");

        install_binary(&source, &destination).expect("install");
        assert_eq!(std::fs::read(&destination).expect("read"), b"#!/bin/true\n");
        let mode = std::fs::metadata(&destination)
            .expect("stat")
            .permissions()
            .mode();
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
