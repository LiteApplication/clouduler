//! Wiring the drive subsystem into the daemon.
//!
//! Three separate jobs, and which of them run depends entirely on what this
//! node's config says it implements:
//!
//! - a bulk transfer listener, so peers can pull manifests and deltas for the
//!   replicated volumes this node holds;
//! - a sync scheduler, walking those volumes and pushing changes out on each
//!   drive's interval;
//! - for served drives, either the server (on the node named `served_by`) or a
//!   FUSE mount of it (on every other node implementing the drive).
//!
//! A node implementing nothing starts none of it and that is a normal setup,
//! not a degraded one.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cl_config::{ClusterConfig, Node};
use cl_drive::{
    drive_port, served, served_port, transport, DeltaEngine, DriveRegistry, IntervalSource,
    MountManager, MountSpec, SyncScheduler,
};
use cl_raft::ids::VolumeId;
use cl_raft::node::ClusterNode;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Everything the drive subsystem started, kept so shutdown can undo it.
///
/// The mounts matter most: a FUSE mount outlives the process that made it, so
/// leaving one behind means the next daemon start finds a mountpoint whose
/// server is gone and every read on it hangs.
pub struct Drives {
    tasks: Vec<JoinHandle<()>>,
    mounts: Option<Arc<MountManager>>,
    mounted: Vec<String>,
}

impl Drives {
    pub fn shutdown(self) {
        for task in self.tasks {
            task.abort();
        }
        if let Some(mounts) = self.mounts {
            for drive in &self.mounted {
                if let Err(e) = mounts.unmount(drive) {
                    warn!(drive = %drive, error = %e, "could not unmount a served drive");
                }
            }
        }
    }
}

pub fn start(
    cfg: &ClusterConfig,
    node: &Node,
    node_name: &str,
    cluster: &Arc<ClusterNode>,
) -> Result<Drives> {
    let mut drives = Drives {
        tasks: Vec::new(),
        mounts: None,
        mounted: Vec::new(),
    };

    if node.drives.is_empty() {
        info!("this node implements no drives");
        return Ok(drives);
    }

    let registry = Arc::new(
        DriveRegistry::new(cfg, node_name).context("building this node's drive registry")?,
    );
    let internal_ip = cfg
        .internal_ip(node)
        .context("node has no address on the internal network")?;

    // Anything this node backs with a real directory has to exist before a
    // container tries to bind mount it. Served drives are excluded: their
    // local path is a mountpoint, created by the mount step below.
    for (name, path) in &node.drives {
        if cfg.drives.get(name).is_some_and(|d| d.is_served()) && cfg.drives[name].served_by.as_deref() != Some(node_name) {
            continue;
        }
        if let Err(e) = std::fs::create_dir_all(path) {
            warn!(drive = %name, path = %path.display(), error = %e, "cannot create the drive directory");
        }
    }

    let replicated: Vec<&String> = node
        .drives
        .keys()
        .filter(|name| !cfg.drives.get(*name).is_some_and(|d| d.is_served()))
        .collect();

    if !replicated.is_empty() {
        let addr = SocketAddr::new(internal_ip, drive_port(node.rpc_port));
        let serving = registry.clone();
        info!(%addr, drives = replicated.len(), "drive transport listening");
        drives.tasks.push(tokio::spawn(async move {
            if let Err(e) = transport::serve(addr, serving).await {
                warn!(error = %e, "drive transport stopped");
            }
        }));

        let scheduler = SyncScheduler::new(cluster.clone(), registry.clone(), Arc::new(DeltaEngine::default()));
        let intervals = DriveIntervals::from_config(cfg);
        drives.tasks.push(tokio::spawn(async move {
            scheduler.run(&intervals).await;
        }));
    }

    start_served(cfg, node, node_name, &mut drives, internal_ip)?;
    Ok(drives)
}

/// Serve the drives this node holds, and mount the ones it does not.
fn start_served(
    cfg: &ClusterConfig,
    node: &Node,
    node_name: &str,
    drives: &mut Drives,
    internal_ip: std::net::IpAddr,
) -> Result<()> {
    let mut specs = Vec::new();

    for (index, (name, drive)) in cfg.drives.iter().enumerate() {
        if !drive.is_served() || !node.implements(name) {
            continue;
        }
        let Some(server_name) = drive.served_by.as_deref() else {
            continue;
        };
        let local = node.drives[name].clone();

        if server_name == node_name {
            let addr = SocketAddr::new(internal_ip, served_port(node.rpc_port, index));
            info!(drive = %name, %addr, path = %local.display(), "serving drive");
            let root = local;
            let drive_name = name.clone();
            drives.tasks.push(tokio::spawn(async move {
                if let Err(e) = served::serve(addr, root).await {
                    warn!(drive = %drive_name, error = %e, "served drive server stopped");
                }
            }));
            continue;
        }

        let Some(server) = cfg.node(server_name) else {
            warn!(drive = %name, server = %server_name, "drive is served by a node that is not in the config");
            continue;
        };
        let Some(server_ip) = cfg.internal_ip(server) else {
            warn!(drive = %name, server = %server_name, "the serving node has no internal address");
            continue;
        };
        specs.push(MountSpec {
            drive: name.clone(),
            mountpoint: local,
            server: SocketAddr::new(server_ip, served_port(server.rpc_port, index)),
        });
    }

    if specs.is_empty() {
        return Ok(());
    }

    // FUSE needs root or `user_allow_other` in /etc/fuse.conf. Refusing to
    // start the whole daemon over it would take out a node that is otherwise
    // healthy and still votes, so this is a warning and the drive is simply
    // unavailable here. The scheduler will not place work needing it, because
    // placement checks the drive is implemented, not that it is mounted.
    if let Err(e) = served::mount::mount_permission() {
        warn!(
            error = %e,
            drives = specs.len(),
            "cannot mount served drives on this node, containers needing them will not run here"
        );
        return Ok(());
    }

    let manager = Arc::new(MountManager::new(tokio::runtime::Handle::current()));
    let names: Vec<String> = specs.iter().map(|s| s.drive.clone()).collect();
    match manager.mount_all(specs) {
        Ok(()) => {
            info!(drives = ?names, "mounted served drives");
            drives.mounted = names;
            drives.mounts = Some(manager);
        }
        Err(e) => warn!(error = %e, "could not mount the served drives"),
    }
    Ok(())
}

/// Per drive sync intervals, straight out of the config.
struct DriveIntervals {
    by_drive: BTreeMap<String, Duration>,
    default: Duration,
}

impl DriveIntervals {
    fn from_config(cfg: &ClusterConfig) -> Self {
        Self {
            by_drive: cfg
                .drives
                .iter()
                .map(|(name, drive)| (name.clone(), drive.sync_interval))
                .collect(),
            // Only reachable for a volume on a drive the config no longer
            // declares, which is a stale assignment being cleaned up.
            default: Duration::from_secs(15 * 60),
        }
    }
}

impl IntervalSource for DriveIntervals {
    fn interval_for(&self, volume: &VolumeId) -> Duration {
        self.by_drive
            .get(&volume.drive)
            .copied()
            .unwrap_or(self.default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_config::ClusterConfig;

    fn config() -> ClusterConfig {
        ClusterConfig::parse(
            r#"
cluster: { name: home }
networks:
  internal: { type: internal, subnet: 10.10.0.0/24, domain: int.example.com }
drives:
  bulk: { sync_interval: 1h }
  fast: { sync_interval: 30s }
nodes:
  n1:
    role: manager
    network: { internal: 10.10.0.1 }
    drives: { fast: /srv/fast, bulk: /srv/bulk }
web: { address: cloud, network: internal }
"#,
        )
        .expect("valid config")
    }

    #[test]
    fn each_drive_syncs_on_its_own_interval() {
        let intervals = DriveIntervals::from_config(&config());
        assert_eq!(
            intervals.interval_for(&VolumeId::new("fast", "data")),
            Duration::from_secs(30)
        );
        assert_eq!(
            intervals.interval_for(&VolumeId::new("bulk", "data")),
            Duration::from_secs(3600)
        );
    }

    #[test]
    fn a_volume_on_an_undeclared_drive_falls_back_rather_than_spinning() {
        let intervals = DriveIntervals::from_config(&config());
        assert_eq!(
            intervals.interval_for(&VolumeId::new("gone", "data")),
            Duration::from_secs(15 * 60)
        );
    }
}
