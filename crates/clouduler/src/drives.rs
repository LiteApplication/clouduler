//! Wiring the drive subsystem into the daemon.
//!
//! Three separate jobs, and which of them run depends entirely on what this
//! node's config says it implements:
//!
//! - a bulk transfer listener, so peers can pull manifests and deltas for the
//!   replicated volumes this node holds, and so a served drive's other
//!   `served_by` candidates can pull the backup copy `SyncScheduler` pushes
//!   them;
//! - a sync scheduler, walking those volumes and pushing changes out on each
//!   drive's interval;
//! - for served drives, the server runs on every `served_by` candidate (only
//!   the active one is actually pointed at by mounting clients), and a FUSE
//!   mount of whichever candidate is currently active runs on every other
//!   node implementing the drive. A background watcher repoints those mounts
//!   when the active candidate changes.
//!
//! A node implementing nothing starts none of it and that is a normal setup,
//! not a degraded one.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
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
    /// This node's drive-name-to-directory map, kept so migration can resolve
    /// a volume to a real path. `None` when the node implements no drive.
    registry: Option<Arc<DriveRegistry>>,
}

impl Drives {
    pub fn registry(&self) -> Option<Arc<DriveRegistry>> {
        self.registry.clone()
    }

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
        registry: None,
    };

    if node.drives.is_empty() {
        info!("this node implements no drives");
        return Ok(drives);
    }

    let registry = Arc::new(
        DriveRegistry::new(cfg, node_name).context("building this node's drive registry")?,
    );
    drives.registry = Some(registry.clone());
    let internal_ip = cfg
        .internal_ip(node)
        .context("node has no address on the internal network")?;

    // Anything this node backs with a real directory has to exist before a
    // container tries to bind mount it. A drive this node only mounts is
    // excluded: its local path is a mountpoint, created by the mount step
    // below, not a directory this node owns.
    for (name, path) in &node.drives {
        let is_served = cfg.drives.get(name).is_some_and(|d| d.is_served());
        let is_mount_only = is_served && !cfg.drives[name].served_by.iter().any(|s| s == node_name);
        if is_mount_only {
            continue;
        }
        if let Err(e) = std::fs::create_dir_all(path) {
            warn!(drive = %name, path = %path.display(), error = %e, "cannot create the drive directory");
        }
    }

    // Every drive this node backs with a real local copy needs the bulk
    // transfer listener and sync scheduler: a replicated drive's peers pull
    // deltas from it, and a served drive's other `served_by` candidates do
    // the same once this node is (or was) the active server.
    let local_copy: Vec<&String> = node
        .drives
        .keys()
        .filter(|name| {
            !cfg.drives.get(*name).is_some_and(|d| d.is_served())
                || cfg.drives[*name].served_by.iter().any(|s| s == node_name)
        })
        .collect();

    if !local_copy.is_empty() {
        let addr = SocketAddr::new(internal_ip, drive_port(node.rpc_port));
        let serving = registry.clone();
        info!(%addr, drives = local_copy.len(), "drive transport listening");
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

    start_served(cfg, node, node_name, &mut drives, internal_ip, cluster)?;
    Ok(drives)
}

/// Serve the drives this node is a `served_by` candidate for (every
/// candidate runs the server, not just the active one), and mount the drives
/// it only uses, pointed at whichever candidate is active right now.
fn start_served(
    cfg: &ClusterConfig,
    node: &Node,
    node_name: &str,
    drives: &mut Drives,
    internal_ip: IpAddr,
    cluster: &Arc<ClusterNode>,
) -> Result<()> {
    let mut specs = Vec::new();
    // Every resolved candidate address per served drive this node mounts,
    // so the failover watcher can repoint a mount without re-reading `cfg`.
    let mut candidate_addrs: BTreeMap<String, BTreeMap<String, SocketAddr>> = BTreeMap::new();

    for (index, (name, drive)) in cfg.drives.iter().enumerate() {
        if !drive.is_served() || !node.implements(name) || drive.served_by.is_empty() {
            continue;
        }
        let local = node.drives[name].clone();

        if drive.served_by.iter().any(|s| s == node_name) {
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

        let mut addrs = BTreeMap::new();
        for server_name in &drive.served_by {
            let Some(server) = cfg.node(server_name) else {
                warn!(drive = %name, server = %server_name, "drive is served by a node that is not in the config");
                continue;
            };
            let Some(server_ip) = cfg.internal_ip(server) else {
                warn!(drive = %name, server = %server_name, "the serving node has no internal address");
                continue;
            };
            addrs.insert(server_name.clone(), SocketAddr::new(server_ip, served_port(server.rpc_port, index)));
        }
        // Best guess for the mount at startup: the highest priority
        // candidate that resolved. Wrong only until the failover watcher's
        // first tick, which corrects it if a lower priority candidate is
        // actually the one currently reachable.
        let Some(initial) = drive.served_by.iter().find_map(|s| addrs.get(s).copied()) else {
            warn!(drive = %name, "none of this drive's serving candidates could be resolved");
            continue;
        };
        specs.push(MountSpec { drive: name.clone(), mountpoint: local, server: initial });
        candidate_addrs.insert(name.clone(), addrs);
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
            drives.mounts = Some(manager.clone());

            let served_by: BTreeMap<String, Vec<String>> =
                candidate_addrs.keys().map(|name| (name.clone(), cfg.drives[name].served_by.clone())).collect();
            let watch_manager = manager.clone();
            let watch_cluster = cluster.clone();
            drives.tasks.push(tokio::spawn(async move {
                watch_active_servers(watch_cluster, watch_manager, served_by, candidate_addrs).await;
            }));
            drives.tasks.push(tokio::spawn(async move {
                manager.supervise(Duration::from_secs(30)).await;
            }));
        }
        Err(e) => warn!(error = %e, "could not mount the served drives"),
    }
    Ok(())
}

/// Repoints each served drive's mount at whichever `served_by` candidate is
/// currently active (the highest priority one that's reachable), so a
/// mounting node keeps working when the server it was pointed at goes down,
/// and switches back once a higher priority candidate recovers.
async fn watch_active_servers(
    cluster: Arc<ClusterNode>,
    manager: Arc<MountManager>,
    served_by: BTreeMap<String, Vec<String>>,
    candidate_addrs: BTreeMap<String, BTreeMap<String, SocketAddr>>,
) {
    let mut current: BTreeMap<String, String> = BTreeMap::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(10));
    loop {
        ticker.tick().await;
        let state = cluster.state().await;

        for (drive, candidates) in &served_by {
            let Some(active) = candidates.iter().find(|n| !state.unreachable.contains(*n)) else {
                // Every candidate is down; leave the mount pointed where it
                // is, since EIO is exactly the right behaviour and there's
                // nothing better to switch to.
                continue;
            };
            if current.get(drive) == Some(active) {
                continue;
            }
            let Some(&addr) = candidate_addrs.get(drive).and_then(|m| m.get(active)) else {
                continue;
            };
            if manager.update_server(drive, addr) {
                info!(drive = %drive, server = %active, %addr, "served drive failed over to a new active server");
                current.insert(drive.clone(), active.clone());
            }
        }
    }
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
