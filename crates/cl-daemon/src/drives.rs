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
//! - for served drives, the drive's root is exported over NFSv4
//!   (`cl_drive::ensure_export`) on every `served_by` candidate (only the
//!   active one is actually pointed at by mounting clients), and an NFS
//!   mount of whichever candidate is currently active runs on every other
//!   node implementing the drive. A background watcher remounts those mounts
//!   when the active candidate changes: unlike a FUSE client, a kernel NFS mount bakes its server address in at mount time, so
//!   failover means unmount-then-remount rather than a live retarget.
//!
//! A node implementing nothing starts none of it and that is a normal setup,
//! not a degraded one.
//!
//! The whole subsystem is supervised: `start` spawns a task that rebuilds it
//! (tearing down every task, unmounting, and un-exporting, then running the
//! same build this module always has) whenever this node's drive topology —
//! which drives it implements, and any served drive's `served_by` list —
//! actually changes in the live config. A drive's `sync_interval` does not
//! trigger a rebuild; it is read live by `DriveIntervals` on every sync pass
//! instead, since that is a far more common edit than adding or removing a
//! whole drive and does not need anything torn down. Adding a drive, or
//! changing who serves one, both now take effect without a restart; removing
//! a drive never deletes the directory it backed, only stops
//! mounting/exporting/transporting it.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cl_config::{ClusterConfig, Node};
use cl_drive::{
    drive_port, transport, BoxFuture, DeltaEngine, DriveRegistry, HookOutcome, IntervalSource,
    MountManager, MountSpec, SyncHooks, SyncPhase, SyncScheduler,
};
use cl_raft::ids::VolumeId;
use cl_raft::node::ClusterNode;
use cl_raft::state::BackupDestinationKind;
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
    let drives = cfg
        .node(node_name)
        .map(|n| n.drives.clone())
        .unwrap_or_default();
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

/// How long a drive transport or served-drive listener waits before binding
/// again after its listener stopped. At boot, this node's own overlay
/// interface can still be coming up when the drive subsystem starts, so a
/// bind can fail transiently; without a retry that listener is gone for good
/// until the next topology change or daemon restart.
const BIND_RETRY: Duration = Duration::from_secs(10);

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
    /// Served-drive roots this build exported, paired with the client
    /// subnet they were exported to, so `teardown` can un-export them.
    exports: Vec<(PathBuf, String)>,
}

impl Running {
    /// Stop every task, unmount every served drive volume this build
    /// mounted, and remove every NFS export it added. A mount or an export
    /// both outlive the process that made them: leaving a mount behind means
    /// the next pass (or the next daemon start) finds a mountpoint whose
    /// server is gone and every read on it hangs; leaving an export behind
    /// means a node that stopped being a `served_by` candidate keeps handing
    /// out a copy nothing is syncing anymore.
    fn teardown(self) {
        for task in self.tasks {
            task.abort();
        }
        if let Some(mounts) = self.mounts {
            if let Err(e) = mounts.unmount_all() {
                warn!(error = %e, "could not unmount every served drive volume");
            }
        }
        for (root, subnet) in self.exports {
            if let Err(e) = cl_drive::remove_export(&root, &subnet) {
                warn!(path = %root.display(), error = %e, "could not remove an NFS export");
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
    keys: cl_raft::KeyWatch,
    docker: Arc<RwLock<Option<cl_docker::Docker>>>,
) -> Drives {
    let boot_cfg = cfg.clone();
    let node_name = node_name.to_string();
    let cluster = cluster.clone();
    let registry: Arc<RwLock<Option<Arc<DriveRegistry>>>> = Arc::default();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let supervisor = {
        let registry = registry.clone();
        let mut config_rx = config_rx;
        let keys = keys;
        let docker = docker;
        tokio::spawn(async move {
            let mut cfg = boot_cfg;
            let mut topology = topology_of(&cfg, &node_name);
            let mut running =
                match build(&cfg, &node_name, &cluster, &registry, &config_rx, &keys, &docker).await {
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
                    changed = config_rx.changed() => {
                        if changed.is_err() {
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
                running =
                    match build(&cfg, &node_name, &cluster, &registry, &config_rx, &keys, &docker).await {
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

    Drives {
        supervisor,
        shutdown_tx,
        registry,
    }
}

/// Keep the registry's staging allowlist equal to the `NodeLocal` backup
/// destinations that point at this node. The transport only stages a peer's
/// volume next to a path on this list, so a peer cannot pick an arbitrary
/// directory to write into. Destinations live in raft state rather than in
/// `cluster.yaml`, so this polls the state the way the other drive watchers
/// do. A destination created a moment ago is rejected until the next tick,
/// which only delays a backup push that its sender retries anyway.
async fn keep_staging_destinations_current(
    cluster: Arc<ClusterNode>,
    registry: Arc<DriveRegistry>,
    node_name: String,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    loop {
        ticker.tick().await;
        let state = cluster.state().await;
        let paths = state
            .restic_destinations
            .values()
            .filter_map(|d| match &d.kind {
                BackupDestinationKind::NodeLocal { node, path } if node == &node_name => {
                    Some(PathBuf::from(path))
                }
                _ => None,
            })
            .collect();
        registry.set_staging_destinations(paths);
    }
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
    keys: &cl_raft::KeyWatch,
    docker: &Arc<RwLock<Option<cl_docker::Docker>>>,
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
        running
            .tasks
            .push(tokio::spawn(keep_staging_destinations_current(
                cluster.clone(),
                registry.clone(),
                node_name.to_string(),
            )));
        let internal_ip = cfg
            .internal_ip(node)
            .context("node has no address on the internal network")?;

        // Anything this node backs with a real directory has to exist before
        // a container tries to bind mount it. A drive this node only mounts
        // is excluded: its local path is a mountpoint, created on demand by
        // `start_volume_mounts`, not a directory this node owns.
        for (name, path) in &node.drives {
            let is_served = cfg.drives.get(name).is_some_and(|d| d.is_served());
            let is_mount_only =
                is_served && !cfg.drives[name].served_by.iter().any(|s| s == node_name);
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
            let serve_keys = keys.clone();
            info!(%addr, drives = local_copy.len(), "drive transport listening");
            running.tasks.push(tokio::spawn(async move {
                loop {
                    if let Err(e) =
                        transport::serve(addr, serving.clone(), serve_keys.clone()).await
                    {
                        warn!(error = %e, "drive transport stopped, retrying");
                    }
                    tokio::time::sleep(BIND_RETRY).await;
                }
            }));

            let scheduler = SyncScheduler::new(
                cluster.clone(),
                registry.clone(),
                Arc::new(DeltaEngine::new(keys.clone())),
            )
            .with_hooks(Arc::new(ActionHooks {
                cluster: cluster.clone(),
                node: node_name.to_string(),
                docker: docker.clone(),
            }));
            let intervals = DriveIntervals {
                config_rx: config_rx.clone(),
            };
            running.tasks.push(tokio::spawn(async move {
                scheduler.run(&intervals).await;
            }));
        }

        running.exports = ensure_serving(cfg, node, node_name);

        let served_drive_names: Vec<String> = cfg
            .drives
            .iter()
            .filter(|(name, drive)| {
                drive.is_served()
                    && node.implements(name)
                    && drive.served_by.iter().any(|s| s == node_name)
            })
            .map(|(name, _)| name.clone())
            .collect();
        if !served_drive_names.is_empty() {
            let watch_cluster = cluster.clone();
            let watch_registry = registry.clone();
            running.tasks.push(tokio::spawn(async move {
                ensure_served_volume_directories(watch_cluster, watch_registry, served_drive_names)
                    .await;
            }));
        }
    }

    start_volume_mounts(cfg, node, node_name, &mut running, cluster)?;
    Ok(running)
}

/// Export every served drive this node is a `served_by` candidate for over
/// NFSv4 (every candidate exports, not just the active one) — the export
/// always covers the whole drive root; which volume a mounting client
/// actually sees is decided client side (`start_volume_mounts`), by mounting
/// a subpath of it.
///
/// Returns the roots this call exported, so the caller can un-export them on
/// the next topology rebuild or on shutdown. A no-op (and an empty list) if
/// this node serves nothing, or if this host can't serve NFS at all
/// (`cl_drive::exports_available`) — logged as a warning, not fatal: the
/// node just never becomes an active server for anything.
fn ensure_serving(cfg: &ClusterConfig, node: &Node, node_name: &str) -> Vec<(PathBuf, String)> {
    let mut exported = Vec::new();

    let serves_something = cfg.drives.iter().any(|(name, drive)| {
        drive.is_served() && node.implements(name) && drive.served_by.iter().any(|s| s == node_name)
    });
    if !serves_something {
        return exported;
    }

    if let Err(e) = cl_drive::exports_available() {
        warn!(error = %e, "cannot serve NFS drives from this node");
        return exported;
    }

    let (_, internal) = cfg.internal_network();
    let Some(subnet) = internal.subnet else {
        warn!("cluster's internal network has no subnet, cannot restrict NFS exports to it");
        return exported;
    };
    let subnet = subnet.to_string();

    for (name, drive) in &cfg.drives {
        if !drive.is_served()
            || !node.implements(name)
            || !drive.served_by.iter().any(|s| s == node_name)
        {
            continue;
        }
        let root = node.drives[name].clone();
        match cl_drive::ensure_export(name, &root, &subnet) {
            Ok(()) => {
                info!(drive = %name, path = %root.display(), %subnet, "exporting served drive over NFS");
                exported.push((root, subnet.clone()));
            }
            Err(e) => warn!(drive = %name, error = %e, "could not export served drive over NFS"),
        }
    }
    exported
}

/// Create the on-disk directory for every volume this node knows about on a
/// served drive it's a `served_by` candidate for.
///
/// Nothing else does this for a served drive. A replicated drive gets it for
/// free: its container always runs on the same node that holds the data, so
/// Docker itself creates a missing bind-mount source directory on container
/// start. A served drive's container usually runs somewhere else entirely,
/// mounting `server:/<volume>` over NFS — and that mount can only succeed if
/// `<volume>` already exists under the export root, which nothing but the
/// serving node itself is in a position to create. Run on every candidate,
/// not just the active server, so a directory a failover promotes into
/// service is already there instead of surfacing as a mount error the
/// moment it takes over.
async fn ensure_served_volume_directories(
    cluster: Arc<ClusterNode>,
    registry: Arc<DriveRegistry>,
    drives: Vec<String>,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(10));
    loop {
        ticker.tick().await;
        let state = cluster.state().await;
        for id in state.volumes.keys() {
            if !drives.contains(&id.drive) {
                continue;
            }
            let already_there = registry.volume_path(id).is_ok_and(|p| p.is_dir());
            match registry.ensure_volume(id) {
                Ok(_) if !already_there => info!(%id, "created served drive volume directory"),
                Ok(_) => {}
                Err(e) => {
                    warn!(%id, error = %e, "could not create a served drive volume's directory")
                }
            }
        }
    }
}

/// One served drive's resolved `served_by` candidates, for deciding which
/// node is the active server and where to reach it, without re-reading
/// `cfg` on every convergence pass. Each candidate's real local directory
/// (`root`) travels with its address: an NFSv4 mount source is the server's
/// *real* filesystem path (see `cl_drive::MountSpec::remote_root`), which a
/// candidate declares independently and so can differ node to node even for
/// the same drive.
struct ServedDriveInfo {
    served_by: Vec<String>,
    candidates: BTreeMap<String, (SocketAddr, PathBuf)>,
}

/// Sets up dynamic, per-volume NFS mounting for every served drive in the
/// cluster — not just ones this node declared in `cluster.yaml`, since a
/// served drive's data is reachable from any node over the internal network.
/// Resolves every candidate's address once, then hands off to a background
/// task that mounts and unmounts individual volumes as this node's own
/// running containers start and stop needing them (`watch_volume_mounts`),
/// rather than mounting a served drive's whole directory up front the way a
/// statically declared `Node.drives` entry does.
///
/// A no-op if this process cannot call `mount(2)` at all (`mount_permission`
/// — in practice, not root), or if the cluster has no served drives whose
/// candidates resolve.
fn start_volume_mounts(
    cfg: &ClusterConfig,
    node: &Node,
    node_name: &str,
    running: &mut Running,
    cluster: &Arc<ClusterNode>,
) -> Result<()> {
    let mut served_drives = BTreeMap::new();
    for (name, drive) in &cfg.drives {
        if !drive.is_served() || drive.served_by.is_empty() {
            continue;
        }
        let mut candidates = BTreeMap::new();
        for server_name in &drive.served_by {
            let Some(server) = cfg.node(server_name) else {
                warn!(drive = %name, server = %server_name, "drive is served by a node that is not in the config");
                continue;
            };
            let Some(server_ip) = cfg.internal_ip(server) else {
                warn!(drive = %name, server = %server_name, "the serving node has no internal address");
                continue;
            };
            let Some(root) = server.drives.get(name) else {
                warn!(drive = %name, server = %server_name, "serving candidate does not declare a local path for this drive");
                continue;
            };
            candidates.insert(
                server_name.clone(),
                (SocketAddr::new(server_ip, cl_drive::NFS_PORT), root.clone()),
            );
        }
        if candidates.is_empty() {
            warn!(drive = %name, "none of this drive's serving candidates could be resolved");
            continue;
        }
        served_drives.insert(
            name.clone(),
            ServedDriveInfo {
                served_by: drive.served_by.clone(),
                candidates,
            },
        );
    }

    if served_drives.is_empty() {
        return Ok(());
    }

    // A raw `mount(2)` call needs root (`CAP_SYS_ADMIN`), and for fstype
    // "nfs4" the kernel's nfsv4 client module has to be loaded first —
    // bypassing the `mount.nfs4` helper (see `served::mount`) means bypassing
    // the autoload it would normally trigger too. Refusing to start the whole
    // daemon over either would take out a node that is otherwise healthy and
    // still votes, so this is only a warning. The watcher below starts anyway
    // and `MountManager::mount_one` re-checks both on every attempt, so a
    // `modprobe nfsv4` after boot takes effect without restarting the daemon.
    // Until then the scheduler still places work needing a served drive here,
    // since placement no longer requires this node to implement one, and
    // those instances just never become ready (see
    // `cl_sched::run::volumes_ready`).
    if let Err(e) = cl_drive::mount_permission() {
        warn!(
            error = %e,
            "cannot mount served drive volumes on this node yet, containers needing one will not run here until it can"
        );
    } else if let Err(e) = cl_drive::client_available() {
        warn!(
            error = %e,
            "cannot mount served drive volumes on this node yet, containers needing one will not run here until it can"
        );
    }

    let manager = Arc::new(MountManager::new());
    running.mounts = Some(manager.clone());

    let data_dir = node.data_dir(cfg).to_path_buf();
    let watch_cluster = cluster.clone();
    let watch_node = node_name.to_string();
    let watch_manager = manager.clone();
    running.tasks.push(tokio::spawn(async move {
        watch_volume_mounts(
            watch_cluster,
            watch_node,
            data_dir,
            served_drives,
            watch_manager,
        )
        .await;
    }));
    running.tasks.push(tokio::spawn(async move {
        manager.supervise(Duration::from_secs(30)).await;
    }));
    Ok(())
}

/// Converges this node's NFS mounts against what its own running containers
/// actually need, one served-drive volume at a time: mounts a volume the
/// moment an instance here needs it and this node isn't the active server,
/// fails it over (unmount, then remount against the new address) if the
/// active server changes, and unmounts it once nothing here needs it any
/// more. Driven off `ClusterState.assignments`
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
    // Mounts whose last attempt failed, so a persistent failure is logged once
    // rather than on every tick, and its recovery is logged too.
    let mut failing: BTreeSet<String> = BTreeSet::new();
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
                let (Some(drive), Some(volume)) = (mount.drive.as_ref(), mount.volume.as_ref())
                else {
                    continue;
                };
                let Some(info) = served_drives.get(drive) else {
                    continue;
                };
                let Some(active) = info
                    .served_by
                    .iter()
                    .map(String::as_str)
                    .find(|n| !state.unreachable.contains(*n))
                else {
                    // No reachable server for this drive right now; nothing
                    // useful to mount against.
                    continue;
                };
                if active == node_name.as_str() {
                    // This node holds the real data; the container reads it
                    // directly, no NFS mount involved.
                    continue;
                }
                let Some((server, remote_root)) = info.candidates.get(active) else {
                    continue;
                };
                let mountpoint = data_dir.join("volume-mounts").join(drive).join(volume);
                let spec = MountSpec {
                    drive: drive.clone(),
                    volume: Some(volume.clone()),
                    mountpoint,
                    server: *server,
                    remote_root: remote_root.clone(),
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
        failing.retain(|key| needed.contains_key(key));
        for (key, spec) in needed {
            if manager.is_mounted(&key) {
                let server = spec.server;
                if let Err(e) = manager.retarget(&key, spec) {
                    warn!(%key, %server, error = %e, "could not fail a served drive mount over to its new server");
                }
            } else {
                if !failing.contains(&key) {
                    info!(drive = %spec.drive, volume = ?spec.volume, server = %spec.server, "mounting served drive volume");
                }
                match manager.mount_one(spec) {
                    Ok(()) => {
                        if failing.remove(&key) {
                            info!(%key, "mounted a served drive volume that had been failing");
                        }
                    }
                    Err(e) => {
                        if failing.insert(key.clone()) {
                            warn!(%key, error = %e, "could not mount a served drive volume, will keep retrying");
                        }
                    }
                }
            }
        }
    }
}

/// Runs a service's `volume_sync` `before`/`after` actions in the containers
/// on this node that mount the volume being replicated.
struct ActionHooks {
    cluster: Arc<ClusterNode>,
    node: String,
    docker: Arc<RwLock<Option<cl_docker::Docker>>>,
}

impl SyncHooks for ActionHooks {
    fn run<'a>(&'a self, volume: &'a VolumeId, phase: SyncPhase) -> BoxFuture<'a, HookOutcome> {
        Box::pin(async move {
            let state = self.cluster.state().await;
            let mut work = Vec::new();
            for (id, assignment) in &state.assignments {
                if assignment.node != self.node
                    || assignment.desired != cl_raft::state::DesiredState::Running
                {
                    continue;
                }
                let Some(stack) = state.stacks.get(&id.stack) else {
                    continue;
                };
                let Ok(file) = cl_config::StackFile::parse(&stack.spec) else {
                    continue;
                };
                let Some(svc) = file.services.get(&id.service) else {
                    continue;
                };
                let mounts = svc.volumes.iter().any(|v| {
                    v.drive.as_deref() == Some(volume.drive.as_str())
                        && v.volume.as_deref() == Some(volume.volume.as_str())
                });
                let Some(ext) = svc.x_clouduler.as_ref().filter(|_| mounts) else {
                    continue;
                };
                let Some(hook) = ext.volume_sync.get(&volume.volume) else {
                    continue;
                };
                let name = match phase {
                    SyncPhase::Before => &hook.before,
                    SyncPhase::After => &hook.after,
                };
                if let Some(action) = name.as_ref().and_then(|n| ext.actions.get(n)) {
                    work.push((id.clone(), name.clone().unwrap_or_default(), action.clone()));
                }
            }
            if work.is_empty() {
                return HookOutcome::None;
            }
            let Some(docker) = self.docker.read().await.clone() else {
                return HookOutcome::Failed("no docker on this node".to_string());
            };
            for (id, name, action) in work {
                if let Err(e) =
                    cl_api::routes::instances::run_action_local(&docker, &id, &action).await
                {
                    return HookOutcome::Failed(format!("action {name:?} on {id}: {e}"));
                }
            }
            HookOutcome::Ok
        })
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
        DriveIntervals {
            config_rx: tokio::sync::watch::channel(Arc::new(cfg)).1,
        }
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
        assert_eq!(
            intervals.interval_for(&VolumeId::new("fast", "data")),
            Duration::from_secs(30)
        );

        let mut edited = config();
        edited.drives.get_mut("fast").unwrap().sync_interval = Duration::from_secs(5);
        tx.send(Arc::new(edited)).unwrap();

        assert_eq!(
            intervals.interval_for(&VolumeId::new("fast", "data")),
            Duration::from_secs(5)
        );
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
        added
            .nodes
            .get_mut("n1")
            .unwrap()
            .drives
            .insert("new".to_string(), "/srv/new".into());
        assert_ne!(topology_of(&config(), "n1"), topology_of(&added, "n1"));
    }
}
