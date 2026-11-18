//! Wiring the drive subsystem into the daemon.
//!
//! Which of these jobs run depends on what this node's config says it implements:
//!
//! - a bulk transfer listener, so peers can pull manifests and deltas for the
//!   replicated volumes this node holds, and a served drive's other `served_by`
//!   candidates can pull the backup copy `SyncScheduler` pushes them;
//! - a sync scheduler, pushing changes out on each drive's interval;
//! - for served drives, an NFSv4 export of the drive root
//!   (`cl_drive::ensure_export`) on every `served_by` candidate (only the active
//!   one is pointed at by clients), and an NFS mount of the active candidate on
//!   every other node implementing the drive. A watcher remounts when the active
//!   candidate changes: a kernel NFS mount bakes its server address in, so failover
//!   is unmount-then-remount.
//!
//! A node implementing nothing starts none of it; that is a normal setup.
//!
//! The whole subsystem is supervised: `start` spawns a task that tears everything
//! down (tasks, mounts, exports) and rebuilds it whenever this node's drive
//! topology (which drives it implements, any served drive's `served_by`) changes
//! in the live config. A drive's `sync_interval` does not trigger a rebuild; it
//! is read live by `DriveIntervals` on every sync pass. Removing a drive never
//! deletes the directory it backed, only stops mounting/exporting/transporting
//! it.

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
    /// Where this node's drive transport listens, and which addresses the
    /// served drives are exported to and mounted from: all of it is bound or
    /// written once per build, so moving the cluster onto another network or
    /// subnet has to rebuild.
    listen: Option<(std::net::IpAddr, u16)>,
    internal_subnet: Option<ipnet::IpNet>,
    server_ips: BTreeMap<String, std::net::IpAddr>,
}

fn topology_of(cfg: &ClusterConfig, node_name: &str) -> Topology {
    let drives = cfg
        .node(node_name)
        .map(|n| n.drives.clone())
        .unwrap_or_default();
    let served_by: BTreeMap<String, Vec<String>> = cfg
        .drives
        .iter()
        .filter(|(_, d)| d.is_served())
        .map(|(name, d)| (name.clone(), d.served_by.clone()))
        .collect();
    let listen = cfg
        .node(node_name)
        .and_then(|n| Some((cfg.internal_ip(n)?, drive_port(n.rpc_port))));
    let server_ips = served_by
        .values()
        .flatten()
        .filter_map(|name: &String| Some((name.clone(), cfg.internal_ip(cfg.node(name)?)?)))
        .collect();
    Topology {
        drives,
        served_by,
        listen,
        internal_subnet: cfg.internal_network().and_then(|(_, net)| net.subnet),
        server_ips,
    }
}

/// How often the supervisor checks the live config for a topology change, besides
/// reacting to `config_rx`.
const SUPERVISE_INTERVAL: Duration = Duration::from_secs(10);

/// How long a drive transport or served-drive listener waits before binding
/// again after its listener stopped. At boot, this node's own overlay
/// interface can still be coming up when the drive subsystem starts, so a
/// bind can fail transiently; without a retry that listener is gone for good
/// until the next topology change or daemon restart.
const BIND_RETRY: Duration = Duration::from_secs(10);

/// This node's live drive registry, shared with migration and backups so a
/// drive added after boot is usable without a restart. Empty while the node
/// implements no drive.
#[derive(Clone, Default)]
pub struct RegistryHandle(Arc<RwLock<Option<Arc<DriveRegistry>>>>);

impl RegistryHandle {
    pub async fn get(&self) -> Option<Arc<DriveRegistry>> {
        self.0.read().await.clone()
    }

    async fn set(&self, registry: Option<Arc<DriveRegistry>>) {
        *self.0.write().await = registry;
    }
}

pub struct Drives {
    supervisor: JoinHandle<()>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// This node's drive-name-to-directory map, kept live so migration can
    /// resolve a volume to a real path even after a topology rebuild.
    /// `None` while the node implements no drive.
    registry: RegistryHandle,
}

impl Drives {
    /// A live handle to the current registry, shared with `migration::start` so a
    /// drive added after boot is usable as a migration source or target.
    pub fn registry(&self) -> RegistryHandle {
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

#[derive(Default)]
struct Running {
    tasks: Vec<JoinHandle<()>>,
    mounts: Option<Arc<MountManager>>,
    /// Served-drive roots this build exported, paired with the client
    /// subnet they were exported to, so `teardown` can un-export them.
    exports: Vec<(PathBuf, String)>,
}

impl Running {
    /// Stop every task, unmount every served drive volume this build mounted, and
    /// remove its NFS exports. Both outlive the process: a leftover mount hangs
    /// every read once its server is gone, and a leftover export keeps handing
    /// out a copy nothing syncs anymore.
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

/// Never fails synchronously: a node whose drive subsystem cannot start (missing
/// internal address, bad registry) still votes and serves the ui, running an
/// empty `Running` until the config that fixes it arrives.
pub fn start(
    cfg: &ClusterConfig,
    node_name: &str,
    cluster: &Arc<ClusterNode>,
    config_rx: tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
    keys: cl_raft::KeyWatch,
    docker: cl_docker::DockerHandle,
) -> Drives {
    let boot_cfg = cfg.clone();
    let node_name = node_name.to_string();
    let cluster = cluster.clone();
    let registry = RegistryHandle::default();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let supervisor = {
        let registry = registry.clone();
        let mut config_rx = config_rx;
        let keys = keys;
        let docker = docker;
        tokio::spawn(async move {
            let mut cfg = boot_cfg;
            let mut topology = topology_of(&cfg, &node_name);
            let mut running = match build(
                &cfg, &node_name, &cluster, &registry, &config_rx, &keys, &docker,
            )
            .await
            {
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
                running = match build(
                    &cfg, &node_name, &cluster, &registry, &config_rx, &keys, &docker,
                )
                .await
                {
                    Ok(running) => running,
                    Err(e) => {
                        warn!(error = %e, "drive subsystem did not restart");
                        registry.set(None).await;
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
/// volume next to a listed path, so a peer cannot pick an arbitrary directory to
/// write into. Destinations live in raft state, so this polls it. A destination
/// created a moment ago is rejected until the next tick; the sender retries
/// anyway.
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

async fn build(
    cfg: &ClusterConfig,
    node_name: &str,
    cluster: &Arc<ClusterNode>,
    live_registry: &RegistryHandle,
    config_rx: &tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
    keys: &cl_raft::KeyWatch,
    docker: &cl_docker::DockerHandle,
) -> Result<Running> {
    let mut running = Running::default();

    let Some(node) = cfg.node(node_name) else {
        live_registry.set(None).await;
        return Ok(running);
    };

    if node.drives.is_empty() {
        debug!("this node backs no drives with a local directory");
        live_registry.set(None).await;
    } else {
        let registry = Arc::new(
            DriveRegistry::new(cfg, node_name).context("building this node's drive registry")?,
        );
        live_registry.set(Some(registry.clone())).await;
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

        // Every drive backed by a real local copy needs the bulk transfer
        // listener and sync scheduler: peers pull deltas from it, and a served
        // drive's other candidates do too once this node is (or was) active.
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
            let intervals = DriveIntervals::new(config_rx.clone());
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
            let supervisor = cl_drive::served::failback::ServedSupervisor::new(
                cluster.clone(),
                registry.clone(),
                Arc::new(DeltaEngine::new(keys.clone())),
                Arc::new(DaemonServedHost {
                    config_rx: config_rx.clone(),
                    node: node_name.to_string(),
                }),
            );
            running
                .tasks
                .push(tokio::spawn(async move { supervisor.run().await }));
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

/// What `ServedSupervisor` needs from the daemon: the live config's fail-back
/// mode, and control over this node's NFS export while a drive is handed over.
struct DaemonServedHost {
    config_rx: tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
    node: String,
}

impl DaemonServedHost {
    fn export_target(&self, drive: &str) -> Option<(PathBuf, String)> {
        let cfg = self.config_rx.borrow().clone();
        let root = cfg.node(&self.node)?.drives.get(drive)?.clone();
        let subnet = cfg.internal_network()?.1.subnet?.to_string();
        Some((root, subnet))
    }
}

impl cl_drive::served::failback::ServedHost for DaemonServedHost {
    fn failback(&self, drive: &str) -> cl_config::Failback {
        self.config_rx
            .borrow()
            .drives
            .get(drive)
            .map(|d| d.failback)
            .unwrap_or_default()
    }

    fn suspend_export(&self, drive: &str) {
        if let Some((root, subnet)) = self.export_target(drive) {
            match cl_drive::remove_export(&root, &subnet) {
                Ok(()) => info!(%drive, "stopped exporting the drive for a handover"),
                Err(e) => warn!(%drive, error = %e, "could not stop exporting the drive"),
            }
        }
    }

    fn resume_export(&self, drive: &str) {
        if let Some((root, subnet)) = self.export_target(drive) {
            if let Err(e) = cl_drive::ensure_export(drive, &root, &subnet) {
                warn!(%drive, error = %e, "could not export the drive again after a handover");
            }
        }
    }

    fn peer_addr(&self, node: &str, state: &cl_raft::ClusterState) -> Option<SocketAddr> {
        let addr = state.nodes.get(node)?.rpc_addr;
        Some(SocketAddr::new(addr.ip(), drive_port(addr.port())))
    }
}

/// Export every served drive this node is a `served_by` candidate for over NFSv4
/// (every candidate exports, not just the active one). The export always covers
/// the whole drive root; which volume a client sees is decided client side
/// (`start_volume_mounts`) by mounting a subpath.
///
/// Returns the roots exported, so the caller can un-export them on rebuild or
/// shutdown. A no-op (empty list) if this node serves nothing or the host can't
/// serve NFS (`cl_drive::exports_available`), which only logs a warning.
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

    let Some(subnet) = cfg.internal_network().and_then(|(_, net)| net.subnet) else {
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
/// served drive it is a `served_by` candidate for.
///
/// Nothing else does: a replicated drive's container runs where the data is, so
/// Docker creates a missing bind source, but a served drive's container usually
/// runs elsewhere over NFS, and the mount fails unless `<volume>` already exists
/// under the export root. Run on every candidate so a failover promotion finds
/// the directory already there.
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

/// One served drive's resolved `served_by` candidates, so the active server can
/// be chosen without re-reading `cfg` each pass. Each candidate's real local
/// directory (`root`) travels with its address: an NFSv4 mount source is the
/// server's real filesystem path (see `cl_drive::MountSpec::remote_root`), which
/// can differ node to node for the same drive.
struct ServedDriveInfo {
    served_by: Vec<String>,
    candidates: BTreeMap<String, (SocketAddr, PathBuf)>,
}

/// Sets up per-volume NFS mounting for every served drive in the cluster, not
/// just ones this node declared: served data is reachable from any node over the
/// internal network. Resolves every candidate's address once, then hands off to a
/// task that mounts/unmounts volumes as this node's containers need them
/// (`watch_volume_mounts`).
///
/// A no-op if this process cannot call `mount(2)` (`mount_permission`, in
/// practice not root) or no served drive's candidates resolve.
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

    // A raw `mount(2)` needs root (`CAP_SYS_ADMIN`), and for "nfs4" the kernel's
    // nfsv4 module must be loaded; bypassing `mount.nfs4` (see `served::mount`)
    // bypasses its autoload. Failing the daemon over either would take out an
    // otherwise healthy node, so this only warns. The watcher starts anyway and
    // `MountManager::mount_one` re-checks on every attempt, so a `modprobe nfsv4`
    // takes effect without a restart. Until then instances needing a served drive
    // here never become ready (see `cl_sched::run::volumes_ready`).
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

/// Converges this node's NFS mounts against what its own running containers need,
/// one served-drive volume at a time: mounts when an instance here needs it and
/// this node isn't the active server, remounts against the new address on
/// failover, unmounts when nothing needs it. Driven off
/// `ClusterState.assignments`, the same state `Scheduler::reconcile` reads.
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
                let Some(active) = cl_raft::served::choose_active_server(
                    &info.served_by,
                    &state.unreachable,
                    state.served.get(drive),
                ) else {
                    continue;
                };
                if active == node_name.as_str() {
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

struct ActionHooks {
    cluster: Arc<ClusterNode>,
    node: String,
    docker: cl_docker::DockerHandle,
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
            let Some(docker) = self.docker.get().await else {
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

/// Sync intervals: a stack's `x-clouduler.volume_sync.VOL.interval` when one
/// mounting the volume sets it (never shorter than `MIN_VOLUME_SYNC_INTERVAL`),
/// else the drive's `sync_interval`. Read live from config and replicated stacks
/// on every call, so edits apply on the next sync pass without a restart.
struct DriveIntervals {
    config_rx: tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
    /// Parsed stacks by name, reparsed only when the stack's revision moves.
    stacks: parking_lot::Mutex<ParsedStacks>,
}

type ParsedStacks = std::collections::BTreeMap<String, (u64, Option<Arc<cl_config::StackFile>>)>;

impl DriveIntervals {
    fn new(config_rx: tokio::sync::watch::Receiver<Arc<ClusterConfig>>) -> Self {
        Self {
            config_rx,
            stacks: Default::default(),
        }
    }

    /// Every deployed stack that parses, from the cache, with its name.
    fn parsed(&self, state: &cl_raft::ClusterState) -> Vec<(String, Arc<cl_config::StackFile>)> {
        let mut cache = self.stacks.lock();
        cache.retain(|name, _| state.stacks.contains_key(name));
        state
            .stacks
            .iter()
            .filter_map(|(name, stack)| {
                let entry = cache
                    .entry(name.clone())
                    .or_insert_with(|| (u64::MAX, None));
                if entry.0 != stack.revision {
                    *entry = (
                        stack.revision,
                        cl_config::StackFile::parse(&stack.spec).ok().map(Arc::new),
                    );
                }
                Some((name.clone(), entry.1.clone()?))
            })
            .collect()
    }

    /// The shortest override any stack sets for `volume`, clamped.
    fn stack_override(&self, volume: &VolumeId, state: &cl_raft::ClusterState) -> Option<Duration> {
        self.parsed(state)
            .iter()
            .filter_map(|(_, file)| file.volume_sync_override(&volume.drive, &volume.volume))
            .min()
            .map(|d| d.max(cl_config::MIN_VOLUME_SYNC_INTERVAL))
    }
}

impl IntervalSource for DriveIntervals {
    fn parallelism_for(&self, drive: &str) -> usize {
        self.config_rx
            .borrow()
            .drives
            .get(drive)
            .map_or(1, |d| d.max_parallel_syncs)
    }

    /// The best (lowest) priority among the stacks using the volume's drive
    /// (a `clouduler prioritize`d stack beats all others); a drive no stack
    /// uses syncs last.
    fn priority_for(&self, volume: &VolumeId, state: &cl_raft::ClusterState) -> i32 {
        self.parsed(state)
            .iter()
            .filter(|(_, file)| file.drives_used().contains(&volume.drive))
            .map(|(name, file)| state.effective_priority(name, file.priority()))
            .min()
            .unwrap_or(i32::MAX)
    }

    fn interval_for(&self, volume: &VolumeId, state: &cl_raft::ClusterState) -> Duration {
        self.stack_override(volume, state).unwrap_or_else(|| {
            self.config_rx
                .borrow()
                .drives
                .get(&volume.drive)
                .map(|drive| drive.sync_interval)
                .unwrap_or(DEFAULT_SYNC_INTERVAL)
        })
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
        DriveIntervals::new(tokio::sync::watch::channel(Arc::new(cfg)).1)
    }

    #[test]
    fn each_drive_syncs_on_its_own_interval() {
        let intervals = intervals(config());
        assert_eq!(
            intervals.interval_for(
                &VolumeId::new("fast", "data"),
                &cl_raft::ClusterState::default()
            ),
            Duration::from_secs(30)
        );
        assert_eq!(
            intervals.interval_for(
                &VolumeId::new("bulk", "data"),
                &cl_raft::ClusterState::default()
            ),
            Duration::from_secs(3600)
        );
    }

    #[test]
    fn a_drive_syncs_at_the_best_priority_of_the_stacks_using_it() {
        let intervals = intervals(config());
        let mut state = cl_raft::ClusterState::default();
        let mut deploy = |name: &str, drive: &str, priority: i32| {
            state.apply(&cl_raft::Request::DeployStack {
                name: name.into(),
                spec: format!(
                    "services:\n  s:\n    image: x\n    volumes:\n      - {drive}/data:/d\nx-clouduler:\n  priority: {priority}\n"
                ),
                at: 1,
            });
        };
        deploy("low", "bulk", 5);
        deploy("high", "bulk", -2);
        deploy("other", "fast", 3);
        let prio = |drive: &str| intervals.priority_for(&VolumeId::new(drive, "data"), &state);
        assert_eq!(prio("bulk"), -2);
        assert_eq!(prio("fast"), 3);
        assert_eq!(prio("unused"), i32::MAX);
    }

    #[test]
    fn a_stack_volume_sync_interval_overrides_the_drive_and_is_clamped() {
        let intervals = intervals(config());
        let mut state = cl_raft::ClusterState::default();
        let deploy = |state: &mut cl_raft::ClusterState, interval: &str, at: i64| {
            state.apply(&cl_raft::Request::DeployStack {
                name: "web".into(),
                spec: format!(
                    "services:\n  db:\n    image: x\n    volumes:\n      - bulk/data:/d\n    x-clouduler:\n      volume_sync:\n        data: {{ interval: {interval} }}\n"
                ),
                at,
            });
        };
        let bulk = VolumeId::new("bulk", "data");
        deploy(&mut state, "5m", 1);
        assert_eq!(
            intervals.interval_for(&bulk, &state),
            Duration::from_secs(300)
        );
        // Another volume on the same drive keeps the drive's interval.
        assert_eq!(
            intervals.interval_for(&VolumeId::new("bulk", "other"), &state),
            Duration::from_secs(3600)
        );
        // A redeploy (new revision) is picked up, and too short is raised.
        deploy(&mut state, "1s", 2);
        assert_eq!(
            intervals.interval_for(&bulk, &state),
            cl_config::MIN_VOLUME_SYNC_INTERVAL
        );
    }

    #[test]
    fn a_volume_on_an_undeclared_drive_falls_back_rather_than_spinning() {
        let intervals = intervals(config());
        assert_eq!(
            intervals.interval_for(
                &VolumeId::new("gone", "data"),
                &cl_raft::ClusterState::default()
            ),
            Duration::from_secs(15 * 60)
        );
    }

    #[test]
    fn a_config_update_changes_the_interval_without_rebuilding_anything() {
        let (tx, rx) = tokio::sync::watch::channel(Arc::new(config()));
        let intervals = DriveIntervals::new(rx);
        assert_eq!(
            intervals.interval_for(
                &VolumeId::new("fast", "data"),
                &cl_raft::ClusterState::default()
            ),
            Duration::from_secs(30)
        );

        let mut edited = config();
        edited.drives.get_mut("fast").unwrap().sync_interval = Duration::from_secs(5);
        tx.send(Arc::new(edited)).unwrap();

        assert_eq!(
            intervals.interval_for(
                &VolumeId::new("fast", "data"),
                &cl_raft::ClusterState::default()
            ),
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

    #[test]
    fn topology_changes_when_the_cluster_is_re_addressed() {
        let mut moved = config();
        moved.networks.get_mut("internal").unwrap().subnet = Some("100.64.0.0/10".parse().unwrap());
        moved.nodes.get_mut("n1").unwrap().network.insert(
            "internal".to_string(),
            cl_config::NodeAddress::Ip("100.64.0.1".parse().unwrap()),
        );
        moved.validate().expect("the re-addressed config is valid");
        assert_ne!(topology_of(&config(), "n1"), topology_of(&moved, "n1"));
    }
}
