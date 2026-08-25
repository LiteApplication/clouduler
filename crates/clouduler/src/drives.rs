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
}

impl Running {
    /// Stop every task and unmount every served drive volume this build
    /// mounted. A FUSE mount outlives the process that made it, so leaving
    /// one behind means the next pass (or the next daemon start) finds a
    /// mountpoint whose server is gone and every read on it hangs.
    fn teardown(self) {
        for task in self.tasks {
            task.abort();
        }
        if let Some(mounts) = self.mounts {
            if let Err(e) = mounts.unmount_all() {
                warn!(error = %e, "could not unmount every served drive volume");
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
        debug!("this node backs no drives with a local directory");
        *live_registry.write().await = None;
    } else {
        let registry = Arc::new(
            DriveRegistry::new(cfg, node_name).context("building this node's drive registry")?,
        );
        *live_registry.write().await = Some(registry.clone());
        let internal_ip = cfg
            .internal_ip(node)
            .context("node has no address on the internal network")?;

        // Anything this node backs with a real directory has to exist before
        // a container tries to bind mount it. A drive this node only mounts
        // is excluded: its local path is a mountpoint, created on demand by
        // `start_volume_mounts`, not a directory this node owns.
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
        // transfer listener and sync scheduler: a replicated drive's peers
        // pull deltas from it, and a served drive's other `served_by`
        // candidates do the same once this node is (or was) the active
        // server.
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

        start_serving(cfg, node, node_name, &mut running, internal_ip);
    }

    start_volume_mounts(cfg, node, node_name, &mut running, cluster)?;
    Ok(running)
}

/// Serve every served drive this node is a `served_by` candidate for (every
/// candidate runs the server, not just the active one) — the server always
/// answers for the whole drive root; which volume a mounting client actually
/// sees is decided client side (`start_volume_mounts`).
fn start_serving(cfg: &ClusterConfig, node: &Node, node_name: &str, running: &mut Running, internal_ip: IpAddr) {
    for (index, (name, drive)) in cfg.drives.iter().enumerate() {
        if !drive.is_served() || !node.implements(name) || !drive.served_by.iter().any(|s| s == node_name) {
            continue;
        }
        let root = node.drives[name].clone();
        let addr = SocketAddr::new(internal_ip, served_port(node.rpc_port, index));
        info!(drive = %name, %addr, path = %root.display(), "serving drive");
        let drive_name = name.clone();
        running.tasks.push(tokio::spawn(async move {
            if let Err(e) = served::serve(addr, root).await {
                warn!(drive = %drive_name, error = %e, "served drive server stopped");
            }
        }));
    }
}

/// One served drive's resolved `served_by` candidates, for deciding which
/// node is the active server and where to reach it, without re-reading
/// `cfg` on every convergence pass.
struct ServedDriveInfo {
    served_by: Vec<String>,
    candidate_addrs: BTreeMap<String, SocketAddr>,
}

/// Sets up dynamic, per-volume FUSE mounting for every served drive in the
/// cluster — not just ones this node declared in `cluster.yaml`, since a
/// served drive's data is reachable from any node over the internal network.
/// Resolves every candidate's address once, then hands off to a background
/// task that mounts and unmounts individual volumes as this node's own
/// running containers start and stop needing them (`watch_volume_mounts`),
/// rather than mounting a served drive's whole directory up front the way a
/// statically declared `Node.drives` entry used to.
///
/// A no-op if this process cannot FUSE-mount at all (`mount_permission`), or
/// if the cluster has no served drives whose candidates resolve.
fn start_volume_mounts(
    cfg: &ClusterConfig,
    node: &Node,
    node_name: &str,
    running: &mut Running,
    cluster: &Arc<ClusterNode>,
) -> Result<()> {
    let mut served_drives = BTreeMap::new();
    for (index, (name, drive)) in cfg.drives.iter().enumerate() {
        if !drive.is_served() || drive.served_by.is_empty() {
            continue;
        }
        let mut candidate_addrs = BTreeMap::new();
        for server_name in &drive.served_by {
            let Some(server) = cfg.node(server_name) else {
                warn!(drive = %name, server = %server_name, "drive is served by a node that is not in the config");
                continue;
            };
            let Some(server_ip) = cfg.internal_ip(server) else {
                warn!(drive = %name, server = %server_name, "the serving node has no internal address");
                continue;
            };
            candidate_addrs.insert(server_name.clone(), SocketAddr::new(server_ip, served_port(server.rpc_port, index)));
        }
        if candidate_addrs.is_empty() {
            warn!(drive = %name, "none of this drive's serving candidates could be resolved");
            continue;
        }
        served_drives.insert(name.clone(), ServedDriveInfo { served_by: drive.served_by.clone(), candidate_addrs });
    }

    if served_drives.is_empty() {
        return Ok(());
    }

    // FUSE needs root or `user_allow_other` in /etc/fuse.conf. Refusing to
    // start the whole daemon over it would take out a node that is otherwise
    // healthy and still votes, so this is a warning and served drives are
    // simply unavailable here. The scheduler will still place work needing
    // one, since placement no longer requires this node to implement a
    // served drive; those instances just never become ready (see
    // `cl_sched::run::volumes_ready`) until they land somewhere that can
    // mount it.
    if let Err(e) = served::mount::mount_permission() {
        warn!(
            error = %e,
            "cannot mount served drive volumes on this node, containers needing one will not run here"
        );
        return Ok(());
    }

    let manager = Arc::new(MountManager::new(tokio::runtime::Handle::current()));
    running.mounts = Some(manager.clone());

    let data_dir = node.data_dir(cfg).to_path_buf();
    let watch_cluster = cluster.clone();
    let watch_node = node_name.to_string();
    let watch_manager = manager.clone();
    running.tasks.push(tokio::spawn(async move {
        watch_volume_mounts(watch_cluster, watch_node, data_dir, served_drives, watch_manager).await;
    }));
    running.tasks.push(tokio::spawn(async move {
        manager.supervise(Duration::from_secs(30)).await;
    }));
    Ok(())
}

/// Converges this node's FUSE mounts against what its own running containers
/// actually need, one served-drive volume at a time: mounts a volume the
/// moment an instance here needs it and this node isn't the active server,
/// retargets it if the active server fails over, and unmounts it once
/// nothing here needs it any more. Driven off `ClusterState.assignments`
/// (the leader's placement decisions), the same replicated state
/// `Scheduler::reconcile` reads, so this and the container it mounts for
/// converge from the same source of truth.
async fn watch_volume_mounts(
    cluster: Arc<ClusterNode>,
    node_name: String,
    data_dir: std::path::PathBuf,
    served_drives: BTreeMap<String, ServedDriveInfo>,
    manager: Arc<MountManager>,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(10));
    loop {
        ticker.tick().await;
        let state = cluster.state().await;

        let mut needed: BTreeMap<String, MountSpec> = BTreeMap::new();
        for assignment in state.assignments_on(&node_name) {
            let Some(stack) = state.stacks.get(&assignment.instance.stack) else {
                continue;
            };
            let Ok(file) = cl_config::StackFile::parse(&stack.spec) else {
                continue;
            };
            let Some(service) = file.services.get(&assignment.instance.service) else {
                continue;
            };
            for mount in &service.volumes {
                let (Some(drive), Some(volume)) = (mount.drive.as_ref(), mount.volume.as_ref()) else {
                    continue;
                };
                let Some(info) = served_drives.get(drive) else {
                    continue;
                };
                let Some(active) = info.served_by.iter().map(String::as_str).find(|n| !state.unreachable.contains(*n))
                else {
                    // No reachable server for this drive right now; nothing
                    // useful to mount against.
                    continue;
                };
                if active == node_name.as_str() {
                    // This node holds the real data; the container reads it
                    // directly, no FUSE involved.
                    continue;
                }
                let Some(&server) = info.candidate_addrs.get(active) else {
                    continue;
                };
                let mountpoint = data_dir.join("volume-mounts").join(drive).join(volume);
                let spec = MountSpec {
                    drive: drive.clone(),
                    volume: Some(volume.clone()),
                    mountpoint,
                    server,
                };
                needed.insert(spec.key(), spec);
            }
        }

        for key in manager.mounted_keys() {
            if !needed.contains_key(&key) {
                if let Err(e) = manager.unmount(&key) {
                    warn!(%key, error = %e, "could not unmount a served drive volume no longer needed here");
                }
            }
        }
        for (key, spec) in needed {
            if manager.is_mounted(&key) {
                manager.update_server(&key, spec.server);
            } else {
                info!(drive = %spec.drive, volume = ?spec.volume, server = %spec.server, "mounting served drive volume");
                if let Err(e) = manager.mount_one(spec) {
                    warn!(%key, error = %e, "could not mount a served drive volume");
                }
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
