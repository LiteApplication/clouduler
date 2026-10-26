//! `clouduler backup restore-{drive,volume,service}`: restore a restic
//! snapshot back onto a drive's real directory.
//!
//! Always stops the affected containers first - unlike backup, this isn't
//! configurable: restoring under a live container is unsafe, with no
//! acceptable degraded mode. The three `restore_*` functions are CLI-invoked
//! and one-shot, run directly on the node that holds the drive (matching
//! `folder_import`'s convention of operating on the local machine).
//!
//! [`start_restore_watcher`] is the api's path to the same three functions:
//! `cl-api` cannot call into this binary crate directly (wrong dependency
//! direction - it's a library `clouduler` depends on, not the reverse), so an
//! api handler instead submits a `RestoreRequest` through raft and this
//! per-node watcher claims and executes whichever ones it can serve, the same
//! "submit an intent, the node that can act on it does" pattern
//! `backup_runner` uses for scheduled/manual backups.
//!
//! A restore can be started from any node. Each job (one drive's worth) is
//! carried out by the one node that can: the node holding the authoritative
//! copy when it can read the repository ([`Mode::Local`] - restic restores
//! straight onto the drive), or, when the repository is a `NodeLocal`
//! directory on some other node, that node ([`Mode::Push`] - restic restores
//! into a staging directory next to the repository, which is then pushed to
//! the authoritative node over the drive transport, the same way a backup
//! stages a drive on a `NodeLocal`-elsewhere destination). A CLI run on a
//! node that can do neither submits a `RestoreRequest` and waits for it.
//!
//! ## Which node, and what happens to the replicas
//!
//! The restore must also run on the node holding the authoritative copy: the
//! primary of every volume it touches for a replicated drive, or the active
//! server of a served one (`restore_authority`). Restoring onto a replica
//! would be overwritten by the next sync from the real primary. After a
//! successful restore the volumes' generation is bumped with this node as
//! primary (`Request::TouchVolume`), so every replica re-pulls the restored
//! data rather than a stale one syncing backwards over it - the failure mode
//! described in `clouduler-volume-primary-desync`.
//!
//! ## Ordering and failure
//!
//! Everything that can be checked without touching anything - the node is
//! authoritative, the destination is reachable from here, the snapshot
//! exists and belongs to the drive - is checked before a single container is
//! stopped. Only instances that were running are stopped (and only those are
//! started again); a stop that cannot be confirmed aborts the restore and
//! restarts what was stopped. If `restic restore` itself fails the volume may
//! be half-restored, so the containers are deliberately LEFT STOPPED rather
//! than started on a mix of old and new files that would then replicate
//! outward; the recorded error says so.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::{ClusterConfig, StackFile};
use cl_drive::{drive_port, DeltaEngine, DriveRegistry, ProgressSink, SyncEngine};
use cl_raft::ids::{InstanceId, VolumeId};
use cl_raft::node::ClusterNode;
use cl_raft::state::{
    BackupDestination, BackupDestinationKind, ClusterState, DesiredState, Request, RestoreRequest,
    RestoreScope, RestoreStatus,
};
use cl_restic::restic::{ResticRepo, SnapshotInfo};
use cl_restic::restore::{include_path_for_volume, validate_snapshot_id};
use cl_secret::ClusterKey;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::backup_runner::{currently_running, instances_using_drive};
use crate::backup_secrets::{self, TempFileGuard};
use cl_raft::client::client_for;
use cl_raft::state::instances_touching_volumes;

/// How often every node checks for `Pending` restore requests it can serve.
const RESTORE_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Per-node: claims and executes `Pending` `RestoreRequest`s whose drive this
/// node implements *and* holds the authoritative copy of - see the module doc
/// comment. Runs on every node, not just the leader, the same as
/// `backup_runner::start_runner`: "can serve this drive" is a local fact each
/// node checks about itself, not a decision that needs a single writer.
pub fn start_restore_watcher(
    cfg: ClusterConfig,
    cluster: Arc<ClusterNode>,
    node_name: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(RESTORE_POLL_INTERVAL);
        loop {
            ticker.tick().await;
            let state = cluster.state().await;
            let Ok(registry) = DriveRegistry::new(&cfg, &node_name) else {
                continue;
            };

            for request in state.restore_requests.values() {
                if !matches!(request.status, RestoreStatus::Pending) {
                    continue;
                }
                let Ok(plan) = plan(&state, &request.scope, request.destination_id.as_deref())
                else {
                    continue;
                };
                // Not this node's job unless it can carry out every job:
                // either it is authoritative for the drive and can read the
                // repository, or it holds the `NodeLocal` repository and can
                // push the result to the authoritative node. Every other
                // node simply skips, so exactly one node ever claims.
                if plan
                    .jobs
                    .iter()
                    .any(|job| job_mode(&registry, &state, job).is_none())
                {
                    continue;
                }

                let claim = cluster
                    .submit(Request::SetRestoreStatus {
                        id: request.id.clone(),
                        status: RestoreStatus::Running,
                        at: unix_now(),
                    })
                    .await;
                let Ok(response) = claim else { continue };
                if response.error.is_some() {
                    // Already claimed by another node, or no longer pending.
                    continue;
                }

                let outcome = execute_restore(&cfg, &node_name, request).await;
                let status = match outcome {
                    Ok(()) => RestoreStatus::Succeeded,
                    Err(e) => {
                        warn!(request = %request.id, error = %e, "restore request failed");
                        RestoreStatus::Failed {
                            error: format!("{e:#}"),
                        }
                    }
                };
                let _ = cluster
                    .submit(Request::SetRestoreStatus {
                        id: request.id.clone(),
                        status,
                        at: unix_now(),
                    })
                    .await;
            }
        }
    })
}

async fn execute_restore(
    cfg: &ClusterConfig,
    node_name: &str,
    request: &cl_raft::state::RestoreRequest,
) -> Result<()> {
    restore_scope(
        cfg,
        node_name,
        &request.scope,
        request.destination_id.as_deref(),
        request.snapshot.as_deref(),
        None,
        false,
    )
    .await
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const STOP_POLL_MARGIN: Duration = Duration::from_secs(15);
/// How often a CLI that handed its restore to the cluster checks on it, and
/// how long it waits before saying nobody has claimed it.
const REQUEST_POLL_INTERVAL: Duration = Duration::from_secs(3);
const REQUEST_UNCLAIMED_WARNING: Duration = Duration::from_secs(60);
const STOP_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// What one restore touches, per drive: either the whole drive or specific
/// volumes. Drives that this restore reaches are in `drives`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Footprint {
    /// drive -> `None` for the whole drive, or the volumes restored on it.
    drives: std::collections::BTreeMap<String, Option<Vec<VolumeId>>>,
}

impl Footprint {
    /// Why this node must not execute the restore, or `None` if it may: it
    /// implements every drive and is authoritative for every volume it will
    /// overwrite.
    fn authority_problem(&self, registry: &DriveRegistry, state: &ClusterState) -> Option<String> {
        for (drive, volumes) in &self.drives {
            if !registry.implements(drive) {
                return Some(format!(
                    "node {} does not implement drive {drive}",
                    registry.node()
                ));
            }
            let served_by = registry
                .is_served(drive)
                .then(|| registry.active_server(drive, &state.unreachable));
            let recorded: Vec<(&VolumeId, &str)> = state
                .volumes
                .iter()
                .filter(|(id, _)| {
                    id.drive == *drive
                        && volumes
                            .as_ref()
                            .is_none_or(|wanted| wanted.iter().any(|w| w == *id))
                })
                .map(|(id, v)| (id, v.primary.as_str()))
                .collect();
            if let Some(problem) = authority_problem(registry.node(), drive, served_by, &recorded) {
                return Some(problem);
            }
        }
        None
    }
}

/// Pure core of `Footprint::authority_problem` for one drive: `served_by` is
/// `None` for a replicated drive and `Some(active server)` for a served one;
/// `recorded` is the (volume, primary) pairs the restore overwrites.
fn authority_problem(
    node: &str,
    drive: &str,
    served_by: Option<Option<&str>>,
    recorded: &[(&VolumeId, &str)],
) -> Option<String> {
    match served_by {
        Some(Some(active)) if active == node => None,
        Some(Some(active)) => Some(format!(
            "drive {drive} is served by {active}; restoring must run there, not on {node}"
        )),
        Some(None) => Some(format!("drive {drive} has no reachable active server")),
        None => recorded
            .iter()
            .find(|(_, primary)| *primary != node)
            .map(|(id, primary)| {
                format!(
                    "{node} is not the primary of {id} (the primary is {primary}); restoring onto a replica would be overwritten by the next sync - run the restore on {primary}"
                )
            }),
    }
}

/// The drives/volumes a restore request touches, resolved against cluster
/// state.
fn scope_footprint(state: &ClusterState, scope: &RestoreScope) -> Result<Footprint> {
    let mut drives = std::collections::BTreeMap::new();
    match scope {
        RestoreScope::Drive { drive } => {
            drives.insert(drive.clone(), None);
        }
        RestoreScope::Volume { volume } => {
            drives.insert(
                volume.drive.clone(),
                volume_footprint(std::slice::from_ref(volume)),
            );
        }
        RestoreScope::Service { stack, service } => {
            let volumes = service_volumes(state, stack, service)?;
            if volumes.is_empty() {
                bail!("{stack}/{service} mounts no volumes");
            }
            let mut by_drive: std::collections::BTreeMap<String, Vec<VolumeId>> =
                Default::default();
            for volume in volumes {
                by_drive
                    .entry(volume.drive.clone())
                    .or_default()
                    .push(volume);
            }
            for (drive, volumes) in by_drive {
                drives.insert(drive, volume_footprint(&volumes));
            }
        }
    }
    Ok(Footprint { drives })
}

/// `None` (the whole drive) when any of `volumes` is the drive-root mount
/// `.`, which restores everything; otherwise exactly `volumes`.
fn volume_footprint(volumes: &[VolumeId]) -> Option<Vec<VolumeId>> {
    if volumes.iter().any(|v| v.volume == ".") {
        None
    } else {
        Some(volumes.to_vec())
    }
}

/// Restore an entire drive from its most recent snapshot at `destination_id`
/// (or a specific one, if `snapshot` is given), stopping and restarting
/// every instance that mounts anything on it.
pub async fn restore_drive(
    cfg: &ClusterConfig,
    node_name: &str,
    drive: &str,
    destination_id: &str,
    snapshot: Option<&str>,
    via: Option<&str>,
) -> Result<()> {
    let scope = RestoreScope::Drive {
        drive: drive.to_string(),
    };
    restore_scope(
        cfg,
        node_name,
        &scope,
        Some(destination_id),
        snapshot,
        via,
        true,
    )
    .await
}

/// Restore a single volume out of the drive's whole-drive snapshot, stopping
/// and restarting only the instances that mount that volume (or the whole
/// drive root, which contains it).
pub async fn restore_volume(
    cfg: &ClusterConfig,
    node_name: &str,
    volume: &VolumeId,
    destination_id: &str,
    snapshot: Option<&str>,
    via: Option<&str>,
) -> Result<()> {
    let scope = RestoreScope::Volume {
        volume: volume.clone(),
    };
    restore_scope(
        cfg,
        node_name,
        &scope,
        Some(destination_id),
        snapshot,
        via,
        true,
    )
    .await
}

/// Restore every volume a service uses, grouped by drive (a service can span
/// drives). Only those volumes are restored - not the rest of the drive - and
/// every instance of any service that mounts one of them is stopped for the
/// duration: restoring under a live container, whichever service owns it, is
/// unsafe. `destination_id` disambiguates when a drive backs up to more than
/// one destination; a drive with only one is restored from it regardless.
pub async fn restore_service(
    cfg: &ClusterConfig,
    node_name: &str,
    stack: &str,
    service: &str,
    destination_id: Option<&str>,
    snapshot: Option<&str>,
    via: Option<&str>,
) -> Result<()> {
    let scope = RestoreScope::Service {
        stack: stack.to_string(),
        service: service.to_string(),
    };
    restore_scope(cfg, node_name, &scope, destination_id, snapshot, via, true).await
}

/// Everything a restore needs to know before anything is touched.
struct Plan {
    /// Every instance to stop for the duration.
    instances: Vec<InstanceId>,
    jobs: Vec<Job>,
}

/// Resolves a restore's scope against cluster state into the jobs and the
/// instances that must be down while they run.
fn plan(state: &ClusterState, scope: &RestoreScope, destination_id: Option<&str>) -> Result<Plan> {
    let footprint = scope_footprint(state, scope)?;
    let instances: Vec<InstanceId> = match scope {
        RestoreScope::Drive { drive } => instances_using_drive(state, drive),
        RestoreScope::Volume { volume } => {
            instances_touching_volumes(state, std::slice::from_ref(volume))
        }
        RestoreScope::Service { stack, service } => {
            let volumes = service_volumes(state, stack, service)?;
            let mut instances: BTreeSet<InstanceId> = instances_of_service(state, stack, service)
                .into_iter()
                .collect();
            instances.extend(instances_touching_volumes(state, &volumes));
            instances.into_iter().collect()
        }
    };
    let mut jobs = Vec::new();
    for (drive, volumes) in footprint.drives {
        let destination = match scope {
            RestoreScope::Service { .. } => {
                resolve_service_destination(state, &drive, destination_id)?
            }
            _ => destination_id
                .ok_or_else(|| anyhow!("a restore of {drive} needs a destination"))?
                .to_string(),
        };
        jobs.push(Job {
            drive,
            destination_id: destination,
            volumes,
        });
    }
    Ok(Plan { instances, jobs })
}

/// What a restore is called in progress messages.
fn scope_subject(scope: &RestoreScope) -> String {
    match scope {
        RestoreScope::Drive { drive } => drive.clone(),
        RestoreScope::Volume { volume } => volume.to_string(),
        RestoreScope::Service { stack, service } => format!("{stack}/{service}"),
    }
}

/// The one restore entry point. Runs the restore here when this node can
/// carry out every job (see [`job_mode`]); otherwise, when `may_submit`,
/// hands it to the cluster as a `RestoreRequest` for the node that can and
/// waits for the outcome - so the CLI works from any node. The request
/// watcher passes `may_submit = false`: it has already decided it can serve
/// the request, and resubmitting would loop.
async fn restore_scope(
    cfg: &ClusterConfig,
    node_name: &str,
    scope: &RestoreScope,
    destination_id: Option<&str>,
    snapshot: Option<&str>,
    via: Option<&str>,
    may_submit: bool,
) -> Result<()> {
    let ctx = Ctx::new(cfg, node_name, via)?;
    let state = ctx
        .client
        .state_with_secrets()
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let Plan { instances, jobs } = plan(&state, scope, destination_id)?;
    let subject = scope_subject(scope);
    if jobs.is_empty() {
        println!("{subject} mounts no volumes; nothing to restore");
        return Ok(());
    }

    if jobs
        .iter()
        .any(|job| job_mode(&ctx.registry, &state, job).is_none())
    {
        if !may_submit {
            bail!("{}", ctx.job_problems(&state, &jobs));
        }
        return ctx.submit_and_wait(scope, destination_id, snapshot).await;
    }

    for job in &jobs {
        println!(
            "restoring {subject} ({}) from {}",
            job.drive, job.destination_id
        );
    }
    ctx.run(&state, instances, jobs, snapshot).await?;
    println!("restored {subject}");
    Ok(())
}

/// How this node can carry out one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// This node holds the authoritative copy and can read the repository:
    /// restic restores straight onto the drive.
    Local,
    /// The repository is a `NodeLocal` directory on this node, but the
    /// authoritative copy lives elsewhere: restic restores into a staging
    /// directory here and the result is pushed to the authoritative node
    /// over the drive transport.
    Push,
}

/// Whether the repository at `destination` can be opened from `node`: a
/// `NodeLocal` directory is only on its own node, everything else is reached
/// over the network. An unknown destination counts as readable so the
/// attempt fails with the precise error rather than waiting forever.
fn destination_readable_from(state: &ClusterState, destination_id: &str, node: &str) -> bool {
    match state
        .restic_destinations
        .get(destination_id)
        .map(|d| &d.kind)
    {
        Some(BackupDestinationKind::NodeLocal { node: holder, .. }) => holder == node,
        _ => true,
    }
}

/// How `registry`'s node can carry out `job`, or `None` if it can't. Exactly
/// one node qualifies for a job: the authority when it can read the
/// repository, else the node holding the `NodeLocal` repository.
fn job_mode(registry: &DriveRegistry, state: &ClusterState, job: &Job) -> Option<Mode> {
    let holds_repo = matches!(
        state.restic_destinations.get(&job.destination_id).map(|d| &d.kind),
        Some(BackupDestinationKind::NodeLocal { node, .. }) if node == registry.node()
    );
    let readable = destination_readable_from(state, &job.destination_id, registry.node());
    let authoritative = Footprint {
        drives: std::iter::once((job.drive.clone(), job.volumes.clone())).collect(),
    }
    .authority_problem(registry, state)
    .is_none();
    if readable && authoritative {
        Some(Mode::Local)
    } else if holds_repo {
        Some(Mode::Push)
    } else {
        None
    }
}

/// Whether a restored volume with no node to receive it can be skipped: only
/// when it was found in the snapshot rather than asked for by name, which is
/// to say it is not recorded in the cluster at all.
fn push_is_optional(volume: &VolumeId, state: &ClusterState) -> bool {
    !state.volumes.contains_key(volume)
}

/// The node that holds the authoritative copy of `volume`: the active server
/// of a served drive, otherwise the volume's recorded primary.
fn authority_node(
    registry: &DriveRegistry,
    state: &ClusterState,
    volume: &VolumeId,
) -> Result<String> {
    if registry.is_served(&volume.drive) {
        return registry
            .active_server(&volume.drive, &state.unreachable)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("drive {} has no reachable active server", volume.drive));
    }
    state
        .volumes
        .get(volume)
        .map(|v| v.primary.clone())
        .ok_or_else(|| anyhow!("volume {volume} is not known to the cluster, so there is no node to restore it onto"))
}

/// The destination to restore `drive` from: the caller's explicit choice if
/// given, otherwise the sole destination any enabled target on that drive
/// backs up to - ambiguous only when more than one exists, in which case the
/// caller must say which.
fn resolve_service_destination(
    state: &ClusterState,
    drive: &str,
    explicit: Option<&str>,
) -> Result<String> {
    if let Some(id) = explicit {
        return Ok(id.to_string());
    }
    let mut destinations: Vec<&str> = state
        .restic_targets
        .values()
        .filter(|t| t.drive == drive)
        .map(|t| t.destination_id.as_str())
        .collect();
    destinations.sort();
    destinations.dedup();
    match destinations.as_slice() {
        [] => bail!("drive {drive} has no backup target on record; pass --restore-from explicitly"),
        [only] => Ok(only.to_string()),
        many => bail!(
            "drive {drive} backs up to {} destinations ({}); pass --restore-from to pick one",
            many.len(),
            many.join(", ")
        ),
    }
}

fn instances_of_service(state: &ClusterState, stack: &str, service: &str) -> Vec<InstanceId> {
    state
        .assignments
        .values()
        .map(|a| &a.instance)
        .filter(|i| i.stack == stack && i.service == service)
        .cloned()
        .collect()
}

fn service_volumes(state: &ClusterState, stack: &str, service: &str) -> Result<Vec<VolumeId>> {
    let stack_state = state
        .stacks
        .get(stack)
        .ok_or_else(|| anyhow!("stack {stack} not found"))?;
    let file =
        StackFile::parse(&stack_state.spec).with_context(|| format!("parsing stack {stack}"))?;
    let svc = file
        .services
        .get(service)
        .ok_or_else(|| anyhow!("service {service} not found in stack {stack}"))?;
    let mut volumes: Vec<VolumeId> = svc
        .volumes
        .iter()
        .filter_map(|mount| {
            Some(VolumeId::new(
                mount.drive.as_ref()?.clone(),
                mount.volume.as_ref()?.clone(),
            ))
        })
        .collect();
    volumes.sort();
    volumes.dedup();
    Ok(volumes)
}

/// One drive's worth of a restore: where from, and which volumes (`None` =
/// everything on the drive).
struct Job {
    drive: String,
    destination_id: String,
    volumes: Option<Vec<VolumeId>>,
}

/// A `Job` after every pre-flight check: the repo is open, the snapshot is
/// resolved and known to belong to the drive.
struct Prepared {
    drive: String,
    repo: ResticRepo,
    _guards: Vec<TempFileGuard>,
    snapshot_id: String,
    source_dir: String,
    target_dir: PathBuf,
    /// `None` entries restore everything under `source_dir`.
    includes: Vec<Option<String>>,
    /// Volumes whose generation must be bumped afterwards (replicated
    /// drives only), with the node that is primary for each.
    touch: Vec<(VolumeId, String)>,
    /// Set for a [`Mode::Push`] job: `target_dir` is a staging directory and
    /// what lands there still has to be pushed to the authoritative node.
    push: Option<PushJob>,
}

/// The second half of a [`Mode::Push`] job.
struct PushJob {
    /// `None` pushes every volume directory the snapshot restored.
    volumes: Option<Vec<VolumeId>>,
}

/// Shared plumbing every restore flow needs: the cluster client, this node's
/// drive registry, and the cluster key to open sealed backup secrets with.
struct Ctx {
    client: cl_raft::client::Client,
    registry: DriveRegistry,
    key: ClusterKey,
    node_name: String,
    data_dir: std::path::PathBuf,
    keys: cl_raft::KeyWatch,
}

impl Ctx {
    fn new(cfg: &ClusterConfig, node_name: &str, via: Option<&str>) -> Result<Self> {
        let node = cfg
            .node(node_name)
            .ok_or_else(|| anyhow!("node {node_name} is not declared in the config"))?;
        let registry = DriveRegistry::new(cfg, node_name)
            .with_context(|| format!("node {node_name} is not in the cluster config"))?;
        let key = ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
            format!(
                "restoring needs the cluster key at {}. Run this from a node that holds it",
                cfg.secrets.key_file.display()
            )
        })?;
        let client = client_for(cfg, via)?;
        let keys = cl_drive::transport::key_watch_from_config(cfg)?;
        Ok(Self {
            client,
            registry,
            keys,
            key,
            node_name: node_name.to_string(),
            data_dir: node.data_dir(cfg).to_path_buf(),
        })
    }

    /// The whole restore: pre-flight, stop, restore, re-sync bookkeeping,
    /// restart - see the module doc comment for the failure policy.
    async fn run(
        &self,
        state: &ClusterState,
        instances: Vec<InstanceId>,
        jobs: Vec<Job>,
        snapshot: Option<&str>,
    ) -> Result<()> {
        // 1. Everything that can fail without side effects, before anything
        //    is stopped.
        let mut modes = Vec::new();
        for job in &jobs {
            match job_mode(&self.registry, state, job) {
                Some(mode) => modes.push(mode),
                None => bail!("{}", self.job_problems(state, std::slice::from_ref(job))),
            }
        }
        if let Some(snapshot) = snapshot {
            validate_snapshot_id(snapshot).map_err(|e| anyhow!(e))?;
        }
        let mut prepared = Vec::new();
        for (job, mode) in jobs.iter().zip(modes) {
            prepared.push(self.prepare(state, job, mode, snapshot).await?);
        }

        // 2. Stop what is running (and only that).
        let running = currently_running(state, &instances);
        println!("stopping {} running instance(s)", running.len());
        let mut stopped: Vec<InstanceId> = Vec::new();
        if let Err(e) = self.stop(state, &instances, &running, &mut stopped).await {
            // Nothing has been touched yet: undo the stop and abort.
            return Err(match self.restart(&stopped).await {
                Ok(()) => e.context("restore aborted before changing any data; containers were restarted"),
                Err(r) => e.context(format!(
                    "restore aborted before changing any data, but restarting the stopped containers failed: {r}"
                )),
            });
        }

        // 3. The restore proper. A failure here may leave the data
        //    half-restored, so containers stay down.
        let mut touch: Vec<(VolumeId, String)> = Vec::new();
        for job in prepared {
            let Prepared {
                drive,
                repo,
                _guards,
                snapshot_id,
                source_dir,
                target_dir,
                includes,
                touch: job_touch,
                push,
            } = job;
            debug!(%drive, snapshot = %snapshot_id, target = %target_dir.display(), "restic restore");
            let target_dir_for_push = target_dir.clone();
            let restored = tokio::task::spawn_blocking(move || -> Result<()> {
                for include in &includes {
                    repo.restore(&snapshot_id, &source_dir, &target_dir, include.as_deref())
                        .map_err(|e| anyhow!("restic restore: {e}"))?;
                }
                Ok(())
            })
            .await
            .map_err(|e| anyhow!("the restic task panicked: {e}"))
            .and_then(|r| r);
            let mut job_touch = job_touch;
            let restored = match (restored, push) {
                (Ok(()), Some(push)) => {
                    let pushed = self
                        .push_staged(state, &drive, &target_dir_for_push, push)
                        .await;
                    pushed.map(|touched| job_touch = touched)
                }
                (restored, _) => restored,
            };
            if let Err(e) = restored {
                let left = if stopped.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; {} container(s) were left STOPPED because {drive} may be half-restored - check the data, re-run the restore, then start them again",
                        stopped.len()
                    )
                };
                return Err(e.context(format!("restoring {drive} failed{left}")));
            }
            touch.extend(job_touch);
        }

        // 4. Make the restored copy the one replicas pull from.
        let touched = self.touch(&touch).await;

        // 5. Bring back what was running.
        let restarted = self.restart(&stopped).await;
        match (touched, restarted) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(t), Ok(())) => Err(anyhow!(
                "the data was restored, but replicas could not be told to re-sync from it: {t}"
            )),
            (Ok(()), Err(r)) => Err(anyhow!(
                "the data was restored, but restarting the stopped containers failed: {r}"
            )),
            (Err(t), Err(r)) => Err(anyhow!(
                "the data was restored, but replicas could not be told to re-sync ({t}) and restarting the containers failed ({r})"
            )),
        }
    }

    /// Resolves destination, repository and snapshot for one job. No
    /// containers are touched.
    async fn prepare(
        &self,
        state: &ClusterState,
        job: &Job,
        mode: Mode,
        requested: Option<&str>,
    ) -> Result<Prepared> {
        let destination: BackupDestination = state
            .restic_destinations
            .get(&job.destination_id)
            .ok_or_else(|| anyhow!("unknown backup destination {}", job.destination_id))?
            .clone();
        let cred_dir = self
            .data_dir
            .join("restic")
            .join("creds")
            .join(&destination.id);
        let (repo, guards) = backup_secrets::build_repo(state, &self.key, &cred_dir, &destination)?;

        let drive = job.drive.clone();
        let requested = requested.map(str::to_string);
        let (repo, info) =
            tokio::task::spawn_blocking(move || -> Result<(ResticRepo, SnapshotInfo)> {
                let info = resolve_snapshot(&repo, requested.as_deref(), &drive)?;
                Ok((repo, info))
            })
            .await
            .map_err(|e| anyhow!("the restic task panicked: {e}"))??;

        let source_dir = match info.paths.as_slice() {
            [only] => only.clone(),
            other => bail!(
                "snapshot {} was taken of {} path(s) ({}); a drive snapshot has exactly one",
                info.id,
                other.len(),
                other.join(", ")
            ),
        };

        let includes: Vec<Option<String>> = match &job.volumes {
            None => vec![None],
            Some(volumes) => volumes
                .iter()
                .map(|v| include_path_for_volume(&v.volume))
                .collect(),
        };
        let (target_dir, touch, push) = match mode {
            Mode::Local => {
                let touch: Vec<(VolumeId, String)> = if self.registry.is_served(&job.drive) {
                    Vec::new()
                } else {
                    state
                        .volumes
                        .keys()
                        .filter(|id| {
                            id.drive == job.drive
                                && job
                                    .volumes
                                    .as_ref()
                                    .is_none_or(|wanted| wanted.iter().any(|w| w == *id))
                        })
                        .map(|id| (id.clone(), self.node_name.clone()))
                        .collect()
                };
                (self.registry.root(&job.drive)?.to_path_buf(), touch, None)
            }
            Mode::Push => {
                // Everything that can be refused is refused before a
                // container is stopped, including a volume nobody can
                // receive.
                for volume in job.volumes.iter().flatten() {
                    authority_node(&self.registry, state, volume)?;
                }
                let BackupDestinationKind::NodeLocal { path, .. } = &destination.kind else {
                    bail!("destination {} is not a local directory", destination.id);
                };
                // Not the backup path's staging directory for the drive: that
                // one is a standing copy that may hold older or extra files,
                // and everything under this one is pushed.
                let staging = self
                    .registry
                    .staging_root_for(Path::new(path), &format!("{}.restore", job.drive));
                match std::fs::remove_dir_all(&staging) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => bail!("clearing {}: {e}", staging.display()),
                }
                std::fs::create_dir_all(&staging)
                    .with_context(|| format!("creating {}", staging.display()))?;
                (
                    staging,
                    Vec::new(),
                    Some(PushJob {
                        volumes: job.volumes.clone(),
                    }),
                )
            }
        };
        Ok(Prepared {
            drive: job.drive.clone(),
            repo,
            _guards: guards,
            snapshot_id: info.id,
            source_dir,
            target_dir,
            includes,
            touch,
            push,
        })
    }

    /// Why this node cannot carry out `jobs`, for an error message.
    fn job_problems(&self, state: &ClusterState, jobs: &[Job]) -> String {
        let mut problems = Vec::new();
        for job in jobs
            .iter()
            .filter(|j| job_mode(&self.registry, state, j).is_none())
        {
            let authority = Footprint {
                drives: std::iter::once((job.drive.clone(), job.volumes.clone())).collect(),
            }
            .authority_problem(&self.registry, state);
            let readable = destination_readable_from(state, &job.destination_id, &self.node_name);
            problems.push(match (authority, readable) {
                (Some(a), false) => format!(
                    "{a}, and destination {} is a local directory on another node",
                    job.destination_id
                ),
                (Some(a), true) => a,
                (None, _) => format!(
                    "destination {} is a local directory on another node",
                    job.destination_id
                ),
            });
        }
        format!(
            "this node cannot carry out the restore: {}",
            problems.join("; ")
        )
    }

    /// Hands the restore to the cluster as a `RestoreRequest` - the node that
    /// can carry it out claims it - and waits for the outcome.
    async fn submit_and_wait(
        &self,
        scope: &RestoreScope,
        destination_id: Option<&str>,
        snapshot: Option<&str>,
    ) -> Result<()> {
        let id = uuid::Uuid::new_v4().to_string();
        let response = self
            .client
            .submit(Request::PutRestoreRequest(RestoreRequest {
                id: id.clone(),
                scope: scope.clone(),
                destination_id: destination_id.map(str::to_string),
                snapshot: snapshot.map(str::to_string),
                requested_at: unix_now(),
                finished_at: None,
                status: RestoreStatus::Pending,
                requested_by: format!("cli on {}", self.node_name),
            }))
            .await
            .map_err(|e| anyhow!("{e}"))?;
        if let Some(error) = response.error {
            bail!("the cluster refused the restore: {error}");
        }
        println!(
            "this node cannot restore {} itself; handed to the cluster as request {id}",
            scope_subject(scope)
        );

        let started = std::time::Instant::now();
        let mut announced_running = false;
        let mut warned_pending = false;
        loop {
            tokio::time::sleep(REQUEST_POLL_INTERVAL).await;
            let state = self.client.state().await.map_err(|e| anyhow!("{e}"))?;
            let request = state.restore_requests.get(&id).ok_or_else(|| {
                anyhow!("restore request {id} disappeared from the cluster state")
            })?;
            match &request.status {
                RestoreStatus::Succeeded => return Ok(()),
                RestoreStatus::Failed { error } => bail!("{error}"),
                RestoreStatus::Running if !announced_running => {
                    announced_running = true;
                    println!("a node claimed the restore and is running it");
                }
                RestoreStatus::Pending
                    if !warned_pending && started.elapsed() > REQUEST_UNCLAIMED_WARNING =>
                {
                    warned_pending = true;
                    println!(
                        "no node has claimed the request yet (is the drive's primary, or the node holding the destination, up?); still waiting - request {id} stays queued"
                    );
                }
                _ => {}
            }
        }
    }

    /// Pushes what a [`Mode::Push`] restore left in `staging` to each
    /// volume's authoritative node, then clears the staging directory.
    /// Returns the volumes whose generation to bump, with their primary.
    async fn push_staged(
        &self,
        state: &ClusterState,
        drive: &str,
        staging: &Path,
        push: PushJob,
    ) -> Result<Vec<(VolumeId, String)>> {
        let volumes: Vec<VolumeId> = match push.volumes {
            Some(volumes) => volumes,
            None => {
                let staging = staging.to_path_buf();
                let drive = drive.to_string();
                tokio::task::spawn_blocking(move || -> Result<Vec<VolumeId>> {
                    let mut found = Vec::new();
                    for entry in std::fs::read_dir(&staging)? {
                        let entry = entry?;
                        if entry.file_type()?.is_dir() {
                            found.push(VolumeId::new(
                                drive.clone(),
                                entry.file_name().to_string_lossy().into_owned(),
                            ));
                        }
                    }
                    found.sort();
                    Ok(found)
                })
                .await
                .map_err(|e| anyhow!("the directory scan panicked: {e}"))??
            }
        };

        let engine = DeltaEngine::new(self.keys.clone());
        let mut touched = Vec::new();
        for volume in volumes {
            let target = match authority_node(&self.registry, state, &volume) {
                Ok(target) => target,
                Err(e) if push_is_optional(&volume, state) => {
                    println!("skipping {volume}: {e}");
                    continue;
                }
                Err(e) => return Err(e),
            };
            let src = cl_drive::resolve_relative(staging, &volume.volume)?;
            let addr = state
                .nodes
                .get(&target)
                .map(|ns| ns.rpc_addr)
                .ok_or_else(|| anyhow!("node {target} is not in the cluster"))?;
            let peer = SocketAddr::new(addr.ip(), drive_port(addr.port()));
            let generation = state.volumes.get(&volume).map_or(0, |v| v.generation);
            println!("sending restored {volume} to {target}");
            engine
                .sync(&src, peer, &volume, generation, None, ProgressSink::none())
                .await
                .map_err(|e| anyhow!("sending {volume} to {target}: {e}"))?;
            if !self.registry.is_served(drive) {
                touched.push((volume, target));
            }
        }
        if let Err(e) = std::fs::remove_dir_all(staging) {
            warn!(path = %staging.display(), error = %e, "could not remove the restore staging directory");
        }
        Ok(touched)
    }

    /// Takes `running` down. Every instance is pushed onto `stopped` before
    /// its stop is requested, so a failure partway still lets the caller
    /// restart exactly what was touched. `all` (running or not) must be
    /// confirmed stopped before this returns.
    async fn stop(
        &self,
        state: &ClusterState,
        all: &[InstanceId],
        running: &[InstanceId],
        stopped: &mut Vec<InstanceId>,
    ) -> Result<()> {
        if all.is_empty() {
            return Ok(());
        }
        for instance in running {
            stopped.push(instance.clone());
        }
        if !running.is_empty() {
            set_desired(&self.client, running, DesiredState::Stopped).await?;
        }
        let deadline = max_stop_grace(state, all) + STOP_POLL_MARGIN;
        wait_for_stopped(&self.client, &self.node_name, all, deadline).await
    }

    /// Sets every instance back to `Running`, attempting all of them even if
    /// one fails.
    async fn restart(&self, stopped: &[InstanceId]) -> std::result::Result<(), String> {
        let mut failures = Vec::new();
        for instance in stopped {
            if let Err(e) = set_desired(
                &self.client,
                std::slice::from_ref(instance),
                DesiredState::Running,
            )
            .await
            {
                failures.push(e.to_string());
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    /// `TouchVolume` for each restored volume with this node as primary:
    /// bumps the generation so every other replica is stale and re-pulls.
    async fn touch(&self, volumes: &[(VolumeId, String)]) -> std::result::Result<(), String> {
        let mut failures = Vec::new();
        for (id, primary) in volumes {
            let response = self
                .client
                .submit(Request::TouchVolume {
                    id: id.clone(),
                    primary: primary.clone(),
                })
                .await;
            match response {
                Ok(r) => {
                    if let Some(error) = r.error {
                        failures.push(format!("{id}: {error}"));
                    }
                }
                Err(e) => failures.push(format!("{id}: {e}")),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
}

/// The snapshot to restore for `drive`. Backups tag every snapshot with the
/// drive's name, and one repository can hold several drives' snapshots, so
/// both "latest" and an explicit id are only accepted among this drive's.
/// `requested` may be `None`/`"latest"` or a (prefix of a) hex snapshot id.
fn resolve_snapshot(
    repo: &ResticRepo,
    requested: Option<&str>,
    drive: &str,
) -> Result<SnapshotInfo> {
    if let Some(id) = requested {
        validate_snapshot_id(id).map_err(|e| anyhow!(e))?;
    }
    let snapshots = repo
        .snapshots()
        .map_err(|e| anyhow!("listing snapshots: {e}"))?;
    pick_snapshot(&snapshots, requested, drive).cloned()
}

fn pick_snapshot<'a>(
    snapshots: &'a [SnapshotInfo],
    requested: Option<&str>,
    drive: &str,
) -> Result<&'a SnapshotInfo> {
    let of_drive: Vec<&SnapshotInfo> = snapshots
        .iter()
        .filter(|s| s.tags.iter().any(|t| t == drive))
        .collect();
    match requested {
        None | Some("latest") => latest_of(&of_drive)
            .ok_or_else(|| anyhow!("this repository has no snapshots of drive {drive} yet")),
        Some(id) => {
            let matches: Vec<&&SnapshotInfo> =
                of_drive.iter().filter(|s| s.id.starts_with(id)).collect();
            match matches.as_slice() {
                [one] => Ok(**one),
                [] => bail!("snapshot {id} is not a snapshot of drive {drive} in this repository"),
                _ => bail!("snapshot id {id} is ambiguous; give more characters"),
            }
        }
    }
}

fn latest_of<'a>(snapshots: &[&'a SnapshotInfo]) -> Option<&'a SnapshotInfo> {
    snapshots.iter().copied().max_by(|a, b| a.time.cmp(&b.time))
}

pub(crate) async fn set_desired(
    client: &cl_raft::client::Client,
    instances: &[InstanceId],
    desired: DesiredState,
) -> Result<()> {
    for instance in instances {
        let response = client
            .submit(Request::SetDesiredState {
                instance: instance.clone(),
                desired,
            })
            .await
            .map_err(|e| anyhow!("{e}"))?;
        if let Some(error) = response.error {
            bail!("could not update the desired state of {instance}: {error}");
        }
    }
    Ok(())
}

pub(crate) fn max_stop_grace(state: &ClusterState, instances: &[InstanceId]) -> Duration {
    let default = cl_sched::run::configured_stop_grace(state);
    instances
        .iter()
        .filter_map(|i| {
            let stack = state.stacks.get(&i.stack)?;
            let file = StackFile::parse(&stack.spec).ok()?;
            file.services.get(&i.service)?.stop_grace_period
        })
        .max()
        .unwrap_or(default)
}

/// Same shape as `folder_import`'s helper of the same name: polls local
/// docker for instances assigned to `node_name`, and the cluster's
/// self-reported `container_status` (see
/// `cl_sched::run::Scheduler::report_container_status`) for anything
/// assigned elsewhere, rather than blindly sleeping out `deadline` for those.
///
/// Fails - rather than returning quietly - if docker cannot be reached for a
/// local container or anything is still not confirmed stopped at `deadline`:
/// restoring under a running container corrupts it.
pub(crate) async fn wait_for_stopped(
    client: &cl_raft::client::Client,
    node_name: &str,
    instances: &[InstanceId],
    deadline: Duration,
) -> Result<()> {
    let mut docker: Option<cl_docker::Docker> = None;
    let start = tokio::time::Instant::now();
    loop {
        let mut not_stopped: Vec<String> = Vec::new();
        match client.state().await {
            Err(e) => not_stopped.push(format!("(cluster state unavailable: {e})")),
            Ok(state) => {
                let local: Vec<String> = state
                    .assignments
                    .values()
                    .filter(|a| a.node == node_name && instances.contains(&a.instance))
                    .map(|a| a.instance.container_name())
                    .collect();
                if !local.is_empty() && docker.is_none() {
                    docker = Some(cl_docker::Docker::connect().map_err(|e| {
                        anyhow!("cannot reach docker to confirm the containers stopped: {e}")
                    })?);
                }
                if let Some(docker) = &docker {
                    for name in &local {
                        match docker.inspect(name).await {
                            Ok(Some(s)) if s.running => not_stopped.push(name.clone()),
                            Ok(_) => {}
                            Err(_) => not_stopped.push(name.clone()),
                        }
                    }
                }
                for instance in instances {
                    if local.contains(&instance.container_name()) {
                        continue;
                    }
                    match state.container_status.get(instance) {
                        Some(status) if !status.running => {}
                        _ => not_stopped.push(instance.container_name()),
                    }
                }
            }
        }
        if not_stopped.is_empty() {
            return Ok(());
        }
        let elapsed = start.elapsed();
        if elapsed >= deadline {
            bail!(
                "could not confirm {} stopped within {}s",
                not_stopped.join(", "),
                deadline.as_secs()
            );
        }
        tokio::time::sleep(STOP_POLL_INTERVAL.min(deadline - elapsed)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: &str, time: &str) -> SnapshotInfo {
        snap_of(id, time, "fast")
    }

    fn snap_of(id: &str, time: &str, drive: &str) -> SnapshotInfo {
        SnapshotInfo {
            id: id.into(),
            time: time.into(),
            tags: vec![drive.into()],
            paths: vec!["/var/lib/clouduler/drives/fast".into()],
        }
    }

    #[test]
    fn latest_of_picks_the_most_recent_by_time() {
        let snapshots = [
            snap("a", "2024-01-01T00:00:00Z"),
            snap("b", "2024-06-01T00:00:00Z"),
            snap("c", "2024-03-01T00:00:00Z"),
        ];
        let refs: Vec<&SnapshotInfo> = snapshots.iter().collect();
        assert_eq!(latest_of(&refs).unwrap().id, "b");
    }

    #[test]
    fn latest_of_an_empty_list_is_none() {
        assert!(latest_of(&[]).is_none());
    }

    #[test]
    fn latest_only_considers_the_drives_own_snapshots() {
        // One repository, two drives: the newest snapshot overall belongs to
        // the other drive and must not be restored onto this one.
        let snapshots = vec![
            snap_of("aaaa1111", "2024-01-01T00:00:00Z", "fast"),
            snap_of("bbbb2222", "2024-09-01T00:00:00Z", "slow"),
        ];
        assert_eq!(
            pick_snapshot(&snapshots, None, "fast").unwrap().id,
            "aaaa1111"
        );
        assert_eq!(
            pick_snapshot(&snapshots, Some("latest"), "slow")
                .unwrap()
                .id,
            "bbbb2222"
        );
        assert!(pick_snapshot(&snapshots, None, "nothing").is_err());
    }

    #[test]
    fn an_explicit_snapshot_must_belong_to_the_drive_and_be_unambiguous() {
        let snapshots = vec![
            snap_of("aaaa1111", "2024-01-01T00:00:00Z", "fast"),
            snap_of("aaaa2222", "2024-02-01T00:00:00Z", "fast"),
            snap_of("bbbb2222", "2024-09-01T00:00:00Z", "slow"),
        ];
        assert_eq!(
            pick_snapshot(&snapshots, Some("aaaa1111"), "fast")
                .unwrap()
                .id,
            "aaaa1111"
        );
        assert!(pick_snapshot(&snapshots, Some("aaaa"), "fast").is_err());
        assert!(pick_snapshot(&snapshots, Some("bbbb2222"), "fast").is_err());
    }

    fn vol(drive: &str, volume: &str) -> VolumeId {
        VolumeId::new(drive.to_string(), volume.to_string())
    }

    #[test]
    fn a_replica_may_not_restore_but_the_primary_may() {
        let v = vol("fast", "db");
        let recorded = [(&v, "n1")];
        assert_eq!(authority_problem("n1", "fast", None, &recorded), None);
        let problem = authority_problem("n2", "fast", None, &recorded).unwrap();
        assert!(
            problem.contains("not the primary") && problem.contains("n1"),
            "{problem}"
        );
        // A volume nobody has recorded yet has no primary to defer to.
        assert_eq!(authority_problem("n2", "fast", None, &[]), None);
    }

    #[test]
    fn a_served_drive_restores_only_on_its_active_server() {
        assert_eq!(authority_problem("n1", "d", Some(Some("n1")), &[]), None);
        assert!(authority_problem("n2", "d", Some(Some("n1")), &[]).is_some());
        assert!(authority_problem("n1", "d", Some(None), &[]).is_some());
    }

    fn destination(id: &str, node: &str) -> BackupDestination {
        BackupDestination {
            id: id.into(),
            name: id.into(),
            kind: BackupDestinationKind::NodeLocal {
                node: node.into(),
                path: "/srv/backups".into(),
            },
            password_secret: format!("_clouduler_restic_password_{id}"),
            credentials_secret: None,
        }
    }

    fn job(drive: &str, destination_id: &str) -> Job {
        Job {
            drive: drive.into(),
            destination_id: destination_id.into(),
            volumes: Some(vec![vol(drive, "db")]),
        }
    }

    /// `db` is primary on `primary`, and the `local` destination lives on
    /// `dest_node`.
    fn restore_state(primary: &str, dest_node: &str) -> ClusterState {
        let mut state = ClusterState::default();
        state
            .restic_destinations
            .insert("local".into(), destination("local", dest_node));
        state.volumes.insert(
            vol("fast", "db"),
            cl_raft::state::VolumeState {
                id: vol("fast", "db"),
                generation: 1,
                primary: primary.into(),
                replicas: Default::default(),
                frozen: None,
                sync_refused: None,
                looks_empty: false,
            },
        );
        state
    }

    fn registry(node: &str) -> DriveRegistry {
        DriveRegistry::from_drives(
            node,
            [("fast".to_string(), PathBuf::from("/srv/fast"))].into(),
        )
    }

    #[test]
    fn the_primary_restores_directly_when_it_can_read_the_repository() {
        let state = restore_state("n1", "n1");
        assert_eq!(
            job_mode(&registry("n1"), &state, &job("fast", "local")),
            Some(Mode::Local)
        );
    }

    #[test]
    fn the_node_holding_a_local_repository_pushes_to_a_primary_elsewhere() {
        // The repository is on n2, the primary is n1.
        let state = restore_state("n1", "n2");
        assert_eq!(
            job_mode(&registry("n2"), &state, &job("fast", "local")),
            Some(Mode::Push)
        );
        // The primary cannot read the repository, and a bystander can do
        // neither.
        assert_eq!(
            job_mode(&registry("n1"), &state, &job("fast", "local")),
            None
        );
        assert_eq!(
            job_mode(&registry("n3"), &state, &job("fast", "local")),
            None
        );
    }

    #[test]
    fn a_repository_reachable_over_the_network_is_restored_by_the_primary_only() {
        let mut state = restore_state("n1", "n2");
        state.restic_destinations.insert(
            "s3".into(),
            BackupDestination {
                kind: BackupDestinationKind::S3 {
                    bucket: "b".into(),
                    endpoint: "https://s3.example.com".into(),
                    region: None,
                    prefix: None,
                },
                ..destination("s3", "n2")
            },
        );
        assert_eq!(
            job_mode(&registry("n1"), &state, &job("fast", "s3")),
            Some(Mode::Local)
        );
        assert_eq!(job_mode(&registry("n2"), &state, &job("fast", "s3")), None);
    }

    #[test]
    fn the_drive_root_volume_widens_a_footprint_to_the_whole_drive() {
        assert_eq!(
            volume_footprint(&[vol("fast", "db"), vol("fast", ".")]),
            None
        );
        assert_eq!(
            volume_footprint(&[vol("fast", "db")]),
            Some(vec![vol("fast", "db")])
        );
    }

    #[test]
    fn a_service_restore_only_names_its_own_volumes_but_stops_every_user_of_them() {
        let mut state = ClusterState::default();
        let spec = |vols: &str| {
            cl_raft::state::StackState {
            name: "app".into(),
            spec: format!("services:\n  web:\n    image: nginx\n    volumes:\n{vols}\n  other:\n    image: nginx\n    volumes:\n      - fast/db:/shared\n"),
            revision: 1,
            configs: Default::default(),
            secrets: Default::default(),
            used_configs: Default::default(),
            used_secrets: Default::default(),
        }
        };
        state.stacks.insert(
            "app".into(),
            spec("      - fast/db:/data\n      - fast/cache:/cache"),
        );
        for service in ["web", "other"] {
            let instance = InstanceId::new("app", service, 0);
            state.assignments.insert(
                instance.clone(),
                cl_raft::state::Assignment {
                    instance,
                    node: "n1".into(),
                    desired: DesiredState::Running,
                    image_digest: None,
                    migration: None,
                },
            );
        }

        let footprint = scope_footprint(
            &state,
            &RestoreScope::Service {
                stack: "app".into(),
                service: "web".into(),
            },
        )
        .unwrap();
        assert_eq!(
            footprint.drives["fast"],
            Some(vec![vol("fast", "cache"), vol("fast", "db")]),
            "only the service's own volumes, never the whole drive"
        );
        // `other` shares fast/db, so it must be down too.
        let touching = instances_touching_volumes(&state, &[vol("fast", "db")]);
        assert_eq!(touching.len(), 2);
    }

    #[test]
    fn resolve_service_destination_passes_through_an_explicit_choice() {
        let state = ClusterState::default();
        assert_eq!(
            resolve_service_destination(&state, "fast", Some("d2")).unwrap(),
            "d2"
        );
    }

    #[test]
    fn resolve_service_destination_requires_disambiguation_when_more_than_one_exists() {
        let mut state = ClusterState::default();
        state.restic_targets.insert(
            "t1".into(),
            cl_raft::state::DriveBackupTarget {
                id: "t1".into(),
                drive: "fast".into(),
                destination_id: "d1".into(),
                enabled: true,
                frequency_secs: 86_400,
                time_of_day: None,
                keep_last: None,
                keep_within_secs: None,
                stop_containers: None,
                last_run: None,
            },
        );
        state.restic_targets.insert(
            "t2".into(),
            cl_raft::state::DriveBackupTarget {
                id: "t2".into(),
                drive: "fast".into(),
                destination_id: "d2".into(),
                enabled: true,
                frequency_secs: 86_400,
                time_of_day: None,
                keep_last: None,
                keep_within_secs: None,
                stop_containers: None,
                last_run: None,
            },
        );
        assert!(resolve_service_destination(&state, "fast", None).is_err());
        assert!(resolve_service_destination(&state, "other", None)
            .unwrap_err()
            .to_string()
            .contains("no backup target"));
    }
}
