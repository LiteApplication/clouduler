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
//!
//! The whole subsystem is supervised: `start` spawns a task that rebuilds it
//! (tearing down every task and unmounting, then running the same build this
//! module always has) whenever this node's drive topology — which drives it
//! implements, and any served drive's `served_by` list — actually changes in
//! the live config. A drive's `sync_interval` does not trigger a rebuild; it
//! is read live by `DriveIntervals` on every sync pass instead, since that is
//! a far more common edit than adding or removing a whole drive and does not
//! need anything torn down. Adding a drive, or changing who serves one, both
//! now take effect without a restart; removing a drive never deletes the
//! directory it backed, only stops mounting/serving/transporting it.

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
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// This node's drive-relevant slice of config: which drives it backs, and
/// (for a served drive) who else may serve it. Two configs that differ only
/// in `sync_interval` or in a part of the cluster this node has nothing to
/// do with compare equal, so the supervisor only rebuilds when something
/// that actually shaped the running tasks changed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Topology {
    drives: BTreeMap<String, std::path::PathBuf>,
    served_by: BTreeMap<String, Vec<String>>,
}

fn topology_of(cfg: &ClusterConfig, node_name: &str) -> Topology {
    let drives = cfg.node(node_name).map(|n| n.drives.clone()).unwrap_or_default();
    let served_by = cfg
        .drives
        .iter()
        .filter(|(_, d)| d.is_served())
        .map(|(name, d)| (name.clone(), d.served_by.clone()))
        .collect();
    Topology { drives, served_by }
}

/// How often the supervisor checks the live config for a topology change, on
/// top of reacting to `config_rx` directly. Matches the order of magnitude of
/// the rest of this daemon's reconcile loops.
const SUPERVISE_INTERVAL: Duration = Duration::from_secs(10);

pub struct Drives {
    supervisor: JoinHandle<()>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// This node's drive-name-to-directory map, kept live so migration can
    /// resolve a volume to a real path even after a topology rebuild.
    /// `None` while the node implements no drive.
    registry: Arc<RwLock<Option<Arc<DriveRegistry>>>>,
}

impl Drives {
    /// A live handle to the current registry, shared with `migration::start`
    /// so a drive added after boot is usable as a migration source or target
    /// without a restart.
    pub fn registry(&self) -> Arc<RwLock<Option<Arc<DriveRegistry>>>> {
        self.registry.clone()
    }

    /// Signal the supervisor to tear everything down and wait for it to
    /// finish, so the mounts are gone before the process that could still
    /// answer them (raft, docker) goes away.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.supervisor.await;
    }
}

/// Everything one build produced, kept only for as long as this build is
/// current — the next topology change tears it all down and replaces it.
#[derive(Default)]
struct Running {
    tasks: Vec<JoinHandle<()>>,
    mounts: Option<Arc<MountManager>>,
    mounted: Vec<String>,
}

impl Running {
    /// Stop every task and unmount every served drive this build mounted. A
    /// FUSE mount outlives the process that made it, so leaving one behind
    /// means the next pass (or the next daemon start) finds a mountpoint
    /// whose server is gone and every read on it hangs.
    fn teardown(self) {
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

/// Never fails synchronously: a node whose drive subsystem cannot start
/// (missing internal address, bad registry) still votes and serves the ui,
/// it just runs an empty `Running` until the config that would fix that
/// arrives, same as any other build failure the supervisor already has to
/// tolerate on a rebuild.
pub fn start(
    cfg: &ClusterConfig,
    node_name: &str,
    cluster: &Arc<ClusterNode>,
    config_rx: tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
) -> Drives {
    let boot_cfg = cfg.clone();
    let node_name = node_name.to_string();
    let cluster = cluster.clone();
    let registry: Arc<RwLock<Option<Arc<DriveRegistry>>>> = Arc::default();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let supervisor = {
        let registry = registry.clone();
        tokio::spawn(async move {
            let mut cfg = boot_cfg;
            let mut topology = topology_of(&cfg, &node_name);
            let mut running = match build(&cfg, &node_name, &cluster, &registry, &config_rx).await {
                Ok(running) => running,
                Err(e) => {
                    warn!(error = %e, "drive subsystem did not start");
                    Running::default()
                }
            };

            let mut ticker = tokio::time::interval(SUPERVISE_INTERVAL);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {}
                    changed = config_rx_changed(&config_rx) => {
                        if !changed {
                            continue;
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_ok() && *shutdown_rx.borrow() {
                            running.teardown();
                            return;
                        }
                    }
                }

                cfg = (**config_rx.borrow()).clone();
                let live_topology = topology_of(&cfg, &node_name);
                if live_topology == topology {
                    continue;
                }
                info!("drive topology changed, rebuilding the drive subsystem");
                topology = live_topology;
                running.teardown();
                running = match build(&cfg, &node_name, &cluster, &registry, &config_rx).await {
                    Ok(running) => running,
                    Err(e) => {
                        warn!(error = %e, "drive subsystem did not restart");
                        *registry.write().await = None;
                        Running::default()
                    }
                };
            }
        })
    };

    Drives { supervisor, shutdown_tx, registry }
}

/// Awaits the next change on `config_rx`, cloning first so the borrow does
/// not outlive the await point (`watch::Receiver::changed` needs `&mut self`,
/// which a shared reference can't give it, so this takes a fresh clone each
/// call — cheap, a `Receiver` is just a pointer and a generation counter).
async fn config_rx_changed(config_rx: &tokio::sync::watch::Receiver<Arc<ClusterConfig>>) -> bool {
    let mut rx = config_rx.clone();
    rx.changed().await.is_ok()
}

/// Build every task and mount this node's current drive topology calls for,
/// same as the daemon has always done at startup — now also reachable from
/// the supervisor's rebuild path, not just from `start`.
async fn build(
    cfg: &ClusterConfig,
    node_name: &str,
    cluster: &Arc<ClusterNode>,
    live_registry: &Arc<RwLock<Option<Arc<DriveRegistry>>>>,
    config_rx: &tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
) -> Result<Running> {
    let mut running = Running::default();

    let Some(node) = cfg.node(node_name) else {
        *live_registry.write().await = None;
        return Ok(running);
    };

    if node.drives.is_empty() {
        debug!("this node implements no drives");
        *live_registry.write().await = None;
        return Ok(running);
    }

    let registry = Arc::new(
        DriveRegistry::new(cfg, node_name).context("building this node's drive registry")?,
    );
    *live_registry.write().await = Some(registry.clone());
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
        running.tasks.push(tokio::spawn(async move {
            if let Err(e) = transport::serve(addr, serving).await {
                warn!(error = %e, "drive transport stopped");
            }
        }));

        let scheduler = SyncScheduler::new(cluster.clone(), registry.clone(), Arc::new(DeltaEngine::default()));
        let intervals = DriveIntervals { config_rx: config_rx.clone() };
        running.tasks.push(tokio::spawn(async move {
            scheduler.run(&intervals).await;
        }));
    }

    start_served(cfg, node, node_name, &mut running, internal_ip, cluster)?;
    Ok(running)
}

/// Serve the drives this node is a `served_by` candidate for (every
/// candidate runs the server, not just the active one), and mount the drives
/// it only uses, pointed at whichever candidate is active right now.
fn start_served(
    cfg: &ClusterConfig,
    node: &Node,
    node_name: &str,
    running: &mut Running,
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
            running.tasks.push(tokio::spawn(async move {
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
            running.mounted = names;
            running.mounts = Some(manager.clone());

            let served_by: BTreeMap<String, Vec<String>> =
                candidate_addrs.keys().map(|name| (name.clone(), cfg.drives[name].served_by.clone())).collect();
            let watch_manager = manager.clone();
            let watch_cluster = cluster.clone();
            running.tasks.push(tokio::spawn(async move {
                watch_active_servers(watch_cluster, watch_manager, served_by, candidate_addrs).await;
            }));
            running.tasks.push(tokio::spawn(async move {
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

/// Only reached for a volume on a drive the live config no longer declares,
/// which is a stale assignment being cleaned up.
const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Per drive sync intervals, read fresh from the live config on every call
/// rather than a snapshot taken once: changing a drive's `sync_interval` in
/// the ui takes effect on the sync scheduler's very next pass, without
/// needing the drive subsystem (or the daemon) to restart.
struct DriveIntervals {
    config_rx: tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
}

impl IntervalSource for DriveIntervals {
    fn interval_for(&self, volume: &VolumeId) -> Duration {
        self.config_rx
            .borrow()
            .drives
            .get(&volume.drive)
            .map(|drive| drive.sync_interval)
            .unwrap_or(DEFAULT_SYNC_INTERVAL)
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
    id: 3b0f6c4a-3d3f-4a8e-9b8b-2c1e6a2a9f01
    role: manager
    network: { internal: 10.10.0.1 }
    drives: { fast: /srv/fast, bulk: /srv/bulk }
web: { address: cloud, network: internal }
"#,
        )
        .expect("valid config")
    }

    fn intervals(cfg: ClusterConfig) -> DriveIntervals {
        DriveIntervals { config_rx: tokio::sync::watch::channel(Arc::new(cfg)).1 }
    }

    #[test]
    fn each_drive_syncs_on_its_own_interval() {
        let intervals = intervals(config());
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
        let intervals = intervals(config());
        assert_eq!(
            intervals.interval_for(&VolumeId::new("gone", "data")),
            Duration::from_secs(15 * 60)
        );
    }

    #[test]
    fn a_config_update_changes_the_interval_without_rebuilding_anything() {
        let (tx, rx) = tokio::sync::watch::channel(Arc::new(config()));
        let intervals = DriveIntervals { config_rx: rx };
        assert_eq!(intervals.interval_for(&VolumeId::new("fast", "data")), Duration::from_secs(30));

        let mut edited = config();
        edited.drives.get_mut("fast").unwrap().sync_interval = Duration::from_secs(5);
        tx.send(Arc::new(edited)).unwrap();

        assert_eq!(intervals.interval_for(&VolumeId::new("fast", "data")), Duration::from_secs(5));
    }

    #[test]
    fn topology_ignores_a_sync_interval_only_change() {
        let mut edited = config();
        edited.drives.get_mut("fast").unwrap().sync_interval = Duration::from_secs(5);
        assert_eq!(topology_of(&config(), "n1"), topology_of(&edited, "n1"));
    }

    #[test]
    fn topology_changes_when_this_nodes_drive_set_changes() {
        let mut added = config();
        added.nodes.get_mut("n1").unwrap().drives.insert("new".to_string(), "/srv/new".into());
        assert_ne!(topology_of(&config(), "n1"), topology_of(&added, "n1"));
    }
}
