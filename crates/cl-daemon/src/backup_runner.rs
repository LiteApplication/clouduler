//! Drives `restic_targets` through to `restic_runs`: decide what is due
//! (leader-side), then, on the node(s) holding the data, stop affected
//! instances if configured to, get the bytes to the destination, run restic,
//! and restart.
//!
//! Two loops, split between "decide" and "do":
//!
//! - [`start_due_checker`] runs only on the leader (a single writer avoids
//!   racing to start the same run), turns an `is_due` target into a `Running`
//!   `BackupRun`, and fails any `Running` run unclaimed for too long. Runs are
//!   not phase-recorded: restic's repository lock already makes a retry safe,
//!   and there is no point-of-no-return step to protect.
//! - [`start_runner`] runs on every node and claims the `Running` runs that are
//!   its job (see `is_local_source` and the `NodeLocal` handling below).
//!
//! ## The `NodeLocal`-elsewhere handoff
//!
//! restic runs as a host process against a real local path, so a `NodeLocal`
//! destination on a node that does not hold the drive needs the data pushed
//! there first (skipped when the destination node already implements the drive,
//! e.g. as a replica). See `push_drive_once`: the same one-off `SyncEngine::sync`
//! as `cl_drive::migration`, deliberately never followed by `SetVolumeReplica`,
//! since this is a point-in-time copy for restic, not a standing replica.
//!
//! The source and destination loops have no direct channel beyond the shared
//! `Running` run: the source pushes once (best effort, guarded in-memory so a
//! slow push is not retried every tick) and the destination, after a fixed grace
//! period from `started_at`, runs restic against whatever has landed. A push
//! still in flight then yields a partial tree for that run; the next scheduled
//! run (or the stale-run watchdog) corrects it.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use cl_config::StackFile;
use cl_drive::{drive_port, DeltaEngine, DriveRegistry, ProgressSink, SyncEngine};
use cl_raft::ids::InstanceId;
use cl_raft::node::ClusterNode;
use cl_raft::state::{
    BackupDestination, BackupDestinationKind, BackupRun, BackupRunOutcome, BackupTrigger,
    ClusterState, DesiredState, DriveBackupTarget, Request,
};
use cl_restic::retention::forget_args;
use cl_secret::ClusterKey;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::warn;
use uuid::Uuid;

use crate::backup_secrets;

const DUE_POLL_INTERVAL: Duration = Duration::from_secs(30);
const RUN_POLL_INTERVAL: Duration = Duration::from_secs(15);
/// A `Running` run whose progress has not moved for `frequency_secs * this`
/// (clamped to `STALE_RUN_FLOOR_SECS`..`STALE_RUN_CAP_SECS`) is presumed
/// abandoned and failed. The clock restarts whenever reported progress changes,
/// since failing a live run would let a second restic hit the same repository;
/// the floor keeps a frequent schedule from condemning a long first backup.
const STALE_RUN_MULTIPLIER: i64 = 3;
const STALE_RUN_FLOOR_SECS: i64 = 12 * 3600;
const STALE_RUN_CAP_SECS: i64 = 72 * 3600;
/// How long a `NodeLocal`-elsewhere destination waits before running restic on
/// whatever the source's one-off push has landed.
const SYNC_GRACE_SECS: i64 = 120;
/// How long a local, non-primary copy of a drive is given to catch up with
/// its primary before a backup runs against it anyway.
const DRIVE_SYNC_GRACE_SECS: i64 = 120;
/// On top of the services' `stop_grace_period`, how long to wait for local
/// containers to stop before failing the run.
const STOP_POLL_MARGIN: Duration = Duration::from_secs(15);

pub fn start_due_checker(cluster: Arc<ClusterNode>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(DUE_POLL_INTERVAL);
        let mut tracker = ProgressTracker::default();
        loop {
            ticker.tick().await;
            if !cluster.is_leader() {
                continue;
            }
            let state = cluster.state().await;
            let now = unix_now();

            for target in state.restic_targets.values() {
                if !cl_restic::schedule::is_due(target, now) {
                    continue;
                }
                // Already owned by a manual or earlier run; skip to avoid a pointless submit each poll.
                let already_running = state.restic_runs.values().any(|r| {
                    r.target_id == target.id
                        && matches!(r.outcome, BackupRunOutcome::Running { .. })
                });
                if already_running {
                    continue;
                }
                let _ = cluster
                    .submit(Request::SetBackupTargetLastRun {
                        id: target.id.clone(),
                        at: now,
                    })
                    .await;
                let run = BackupRun {
                    id: Uuid::new_v4().to_string(),
                    target_id: target.id.clone(),
                    drive: target.drive.clone(),
                    destination_id: target.destination_id.clone(),
                    started_at: now,
                    finished_at: None,
                    outcome: BackupRunOutcome::Running { percent: None },
                    triggered_by: BackupTrigger::Schedule,
                };
                let _ = cluster.submit(Request::RecordBackupRun(run)).await;
            }

            tracker.retain_running(&state);
            for run in state.restic_runs.values() {
                if !matches!(run.outcome, BackupRunOutcome::Running { .. }) {
                    continue;
                }
                let max_age = stale_after_secs(
                    state
                        .restic_targets
                        .get(&run.target_id)
                        .map(|t| t.frequency_secs),
                );
                let last_progress = tracker.last_progress(run, now);
                if now - last_progress >= max_age {
                    let mut stale = run.clone();
                    stale.finished_at = Some(now);
                    stale.outcome = BackupRunOutcome::Failed {
                        error: format!(
                            "interrupted: no progress for {} hours and no node finished this run",
                            (now - last_progress) / 3600
                        ),
                    };
                    let _ = cluster.submit(Request::RecordBackupRun(stale)).await;
                }
            }
        }
    })
}

fn stale_after_secs(frequency_secs: Option<u64>) -> i64 {
    match frequency_secs {
        Some(freq) => (freq.min(i64::MAX as u64 / 4) as i64 * STALE_RUN_MULTIPLIER)
            .clamp(STALE_RUN_FLOOR_SECS, STALE_RUN_CAP_SECS),
        None => STALE_RUN_CAP_SECS,
    }
}

/// Leader-local memory of when each `Running` run last made progress. The
/// raft record only carries restic's `percent_done`, not when it changed, so
/// the leader watches it move. A run seen for the first time (including after
/// a leader change) is credited with progress "now": that can only delay
/// failing a dead run, never fail a live one early.
#[derive(Default)]
struct ProgressTracker {
    seen: std::collections::HashMap<String, (Option<u64>, i64)>,
}

impl ProgressTracker {
    fn last_progress(&mut self, run: &BackupRun, now: i64) -> i64 {
        let percent_bits = match run.outcome {
            BackupRunOutcome::Running { percent } => percent.map(f64::to_bits),
            _ => None,
        };
        let entry = self
            .seen
            .entry(run.id.clone())
            .or_insert((percent_bits, now));
        if entry.0 != percent_bits {
            *entry = (percent_bits, now);
        }
        entry.1
    }

    fn retain_running(&mut self, state: &ClusterState) {
        self.seen.retain(|id, _| {
            state
                .restic_runs
                .get(id)
                .is_some_and(|r| matches!(r.outcome, BackupRunOutcome::Running { .. }))
        });
    }
}

pub fn start_runner(
    cluster: Arc<ClusterNode>,
    node_name: String,
    registry: Arc<tokio::sync::RwLock<Option<Arc<DriveRegistry>>>>,
    docker: cl_docker::DockerHandle,
    key_rx: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
    data_dir: PathBuf,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(RUN_POLL_INTERVAL);
        // Reset on restart, which is harmless: a retried execute is safe (restic's
        // repo lock) and a retried push is cheap (delta-based).
        let in_flight: Arc<Mutex<BTreeSet<String>>> = Arc::default();
        let pushed: Arc<Mutex<BTreeSet<String>>> = Arc::default();

        loop {
            ticker.tick().await;
            let Some(registry) = registry.read().await.clone() else {
                continue;
            };
            let Some(key) = key_rx.borrow().clone() else {
                continue;
            };
            let state = cluster.state().await;

            for run in state.restic_runs.values() {
                if !matches!(run.outcome, BackupRunOutcome::Running { .. }) {
                    continue;
                }
                let Some(target) = state.restic_targets.get(&run.target_id) else {
                    continue;
                };
                let Some(destination) = state.restic_destinations.get(&run.destination_id) else {
                    continue;
                };
                // No blanket `registry.implements` gate: the `NodeLocal { node ==
                // node_name }` branch must run even when this node does not
                // implement the drive, or those runs would stay `Running` forever.
                let is_source = is_local_source(&registry, &state, &run.drive);

                match &destination.kind {
                    BackupDestinationKind::NodeLocal { node, .. } if node == &node_name => {
                        if !is_source && unix_now() - run.started_at < SYNC_GRACE_SECS {
                            continue;
                        }
                        claim_and_execute(
                            &cluster,
                            &registry,
                            &docker,
                            &key,
                            &data_dir,
                            &node_name,
                            &state,
                            run,
                            target,
                            destination,
                            &in_flight,
                        )
                        .await;
                    }
                    BackupDestinationKind::NodeLocal { node, path }
                        if pushes_volumes(&registry, &state, &run.drive) && node != &node_name =>
                    {
                        // A destination that already implements the drive has the data;
                        // its own `NodeLocal` branch runs restic off it after the grace period.
                        if state
                            .nodes
                            .get(node)
                            .is_some_and(|n| n.implements(&run.drive))
                        {
                            continue;
                        }
                        if !pushed.lock().await.insert(run.id.clone()) {
                            continue;
                        }
                        if let Err(e) =
                            push_drive_once(&state, &registry, &run.drive, node, path, &key_rx)
                                .await
                        {
                            warn!(run = %run.id, drive = %run.drive, destination = %node, error = %e, "one-off backup sync failed");
                            // The destination cannot tell an in-flight push from a failed
                            // one; only the source knows. Fail now rather than wait up to
                            // `STALE_RUN_CAP_SECS` for the watchdog.
                            let mut failed = run.clone();
                            failed.finished_at = Some(unix_now());
                            failed.outcome = BackupRunOutcome::Failed {
                                error: format!("syncing {} to {node}: {e}", run.drive),
                            };
                            let _ = cluster.submit(Request::RecordBackupRun(failed)).await;
                        }
                    }
                    BackupDestinationKind::Ssh { .. } | BackupDestinationKind::S3 { .. }
                        if is_source =>
                    {
                        claim_and_execute(
                            &cluster,
                            &registry,
                            &docker,
                            &key,
                            &data_dir,
                            &node_name,
                            &state,
                            run,
                            target,
                            destination,
                            &in_flight,
                        )
                        .await;
                    }
                    _ => {}
                }
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn claim_and_execute(
    cluster: &Arc<ClusterNode>,
    registry: &DriveRegistry,
    docker: &cl_docker::DockerHandle,
    key: &ClusterKey,
    data_dir: &Path,
    node_name: &str,
    state: &ClusterState,
    run: &BackupRun,
    target: &DriveBackupTarget,
    destination: &BackupDestination,
    in_flight: &Arc<Mutex<BTreeSet<String>>>,
) {
    if !in_flight.lock().await.insert(run.id.clone()) {
        return;
    }
    let result = execute(
        cluster,
        registry,
        docker,
        key,
        data_dir,
        node_name,
        state,
        run,
        target,
        destination,
    )
    .await;
    let outcome = match &result {
        Ok(outcome) => outcome.clone(),
        Err(e) => {
            warn!(run = %run.id, drive = %run.drive, error = %e, "backup run failed");
            BackupRunOutcome::Failed {
                error: e.to_string(),
            }
        }
    };
    let mut finished = run.clone();
    finished.finished_at = Some(unix_now());
    finished.outcome = outcome;
    let _ = cluster.submit(Request::RecordBackupRun(finished)).await;
    in_flight.lock().await.remove(&run.id);
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    cluster: &Arc<ClusterNode>,
    registry: &DriveRegistry,
    docker: &cl_docker::DockerHandle,
    key: &ClusterKey,
    data_dir: &Path,
    node_name: &str,
    state: &ClusterState,
    run: &BackupRun,
    target: &DriveBackupTarget,
    destination: &BackupDestination,
) -> Result<BackupRunOutcome> {
    let instances = instances_using_drive(state, &run.drive);
    let should_stop = resolve_stop_containers(target, state, &instances);

    // One an operator stopped on purpose must not be resurrected by a backup.
    let mut stopped: Vec<InstanceId> = Vec::new();
    let result = stop_and_back_up(
        cluster,
        registry,
        docker,
        key,
        data_dir,
        node_name,
        state,
        run,
        target,
        destination,
        if should_stop { &instances } else { &[] },
        &mut stopped,
    )
    .await;

    let restart = restart_instances(cluster, &stopped).await;
    combine_with_restart(result, restart)
}

/// The part of a backup run that happens while containers are down. Each instance
/// is pushed onto `stopped` *before* its stop is requested, so a failure anywhere
/// still leaves a complete list to restart.
#[allow(clippy::too_many_arguments)]
async fn stop_and_back_up(
    cluster: &Arc<ClusterNode>,
    registry: &DriveRegistry,
    docker: &cl_docker::DockerHandle,
    key: &ClusterKey,
    data_dir: &Path,
    node_name: &str,
    state: &ClusterState,
    run: &BackupRun,
    target: &DriveBackupTarget,
    destination: &BackupDestination,
    to_stop: &[InstanceId],
    stopped: &mut Vec<InstanceId>,
) -> Result<BackupRunOutcome> {
    let running = currently_running(state, to_stop);
    // One at a time rather than all `SetDesiredState` at once, so a node does not
    // shut down every affected container in the same instant.
    for instance in &running {
        let one = std::slice::from_ref(instance);
        stopped.push(instance.clone());
        set_desired(cluster, one, DesiredState::Stopped).await?;
        let deadline = max_stop_grace(state, one) + STOP_POLL_MARGIN;
        // A backup of a running database is crash-consistent at best, so an
        // unconfirmed stop aborts the run (the caller restarts everything).
        wait_for_stopped(cluster, node_name, one, docker, deadline).await?;
    }

    // A `NodeLocal`-elsewhere destination does not implement the drive, so restic
    // runs against the staging copy in a hidden directory next to the destination's
    // own path (`DriveRegistry::staging_root_for`), not this node's data dir: the
    // staged copy can be the whole drive. `ensure_staging_root` guarantees an
    // existing directory even if no push has landed yet.
    let source_path = if registry.implements(&run.drive) {
        // A local replica can lag its primary and restic would capture stale data
        // silently. Give it a bounded chance to catch up; one still behind at the
        // deadline is backed up as-is (same trade-off as `SYNC_GRACE_SECS`).
        wait_for_drive_synced(
            cluster,
            registry,
            &run.drive,
            node_name,
            Duration::from_secs(DRIVE_SYNC_GRACE_SECS as u64),
        )
        .await;
        registry.root(&run.drive)?.to_path_buf()
    } else {
        let BackupDestinationKind::NodeLocal { path, .. } = &destination.kind else {
            bail!(
                "destination {} has no local path to stage {} next to",
                destination.id,
                run.drive
            );
        };
        registry.ensure_staging_root(Path::new(path), &run.drive)?
    };
    run_restic_backup(
        cluster,
        key,
        data_dir,
        state,
        run,
        target,
        destination,
        &source_path,
    )
    .await
}

pub fn currently_running(state: &ClusterState, instances: &[InstanceId]) -> Vec<InstanceId> {
    instances
        .iter()
        .filter(|instance| {
            state
                .assignments
                .get(*instance)
                .is_some_and(|a| a.desired == DesiredState::Running)
        })
        .cloned()
        .collect()
}

/// Sets every instance in `stopped` back to `Running`, attempting all of them
/// even if one submit fails, and reporting every failure.
async fn restart_instances(
    cluster: &Arc<ClusterNode>,
    stopped: &[InstanceId],
) -> std::result::Result<(), String> {
    let mut failures = Vec::new();
    for instance in stopped {
        if let Err(e) = set_desired(
            cluster,
            std::slice::from_ref(instance),
            DesiredState::Running,
        )
        .await
        {
            warn!(%instance, error = %e, "could not restart an instance stopped for a backup");
            failures.push(e.to_string());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// Folds the restart result into the backup's: the backup's error wins (restart
/// trouble is appended), and a restart failure after a good snapshot becomes a
/// warning on the success.
fn combine_with_restart(
    result: Result<BackupRunOutcome>,
    restart: std::result::Result<(), String>,
) -> Result<BackupRunOutcome> {
    match (result, restart) {
        (result, Ok(())) => result,
        (
            Ok(BackupRunOutcome::Succeeded {
                snapshot_id,
                bytes,
                warning,
            }),
            Err(restart_error),
        ) => {
            let restart_warning =
                format!("the stopped containers could not be restarted: {restart_error}");
            Ok(BackupRunOutcome::Succeeded {
                snapshot_id,
                bytes,
                warning: Some(match warning {
                    Some(existing) => format!("{existing}; {restart_warning}"),
                    None => restart_warning,
                }),
            })
        }
        (Ok(other), Err(_)) => Ok(other),
        (Err(e), Err(restart_error)) => Err(anyhow!(
            "{e}; additionally the stopped containers could not be restarted: {restart_error}"
        )),
    }
}

/// Instances whose stack mounts a volume on `drive` (a backup covers the whole
/// drive in one restic call).
pub fn instances_using_drive(state: &ClusterState, drive: &str) -> Vec<InstanceId> {
    let mut found = Vec::new();
    for assignment in state.assignments.values() {
        let instance = &assignment.instance;
        let Some(stack) = state.stacks.get(&instance.stack) else {
            continue;
        };
        let Ok(file) = StackFile::parse(&stack.spec) else {
            continue;
        };
        let Some(service) = file.services.get(&instance.service) else {
            continue;
        };
        if service
            .volumes
            .iter()
            .any(|m| m.drive.as_deref() == Some(drive))
        {
            found.push(instance.clone());
        }
    }
    found
}

/// The target's own override wins; otherwise any explicit `true` across the
/// affected services' `x-clouduler` overrides stops, and only a unanimous
/// explicit `false` skips. Default is `true`, the safe choice.
fn resolve_stop_containers(
    target: &DriveBackupTarget,
    state: &ClusterState,
    instances: &[InstanceId],
) -> bool {
    if let Some(explicit) = target.stop_containers {
        return explicit;
    }
    let resolved: Vec<bool> = instances
        .iter()
        .filter_map(|instance| {
            let stack = state.stacks.get(&instance.stack)?;
            let file = StackFile::parse(&stack.spec).ok()?;
            file.effective_backup_stop_before(&instance.service)
        })
        .collect();
    resolved.is_empty() || resolved.into_iter().any(|b| b)
}

fn max_stop_grace(state: &ClusterState, instances: &[InstanceId]) -> Duration {
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

async fn set_desired(
    cluster: &Arc<ClusterNode>,
    instances: &[InstanceId],
    desired: DesiredState,
) -> Result<()> {
    for instance in instances {
        let response = cluster
            .submit(Request::SetDesiredState {
                instance: instance.clone(),
                desired,
            })
            .await?;
        if let Some(error) = response.error {
            bail!("could not update the desired state of {instance}: {error}");
        }
    }
    Ok(())
}

/// Polls local docker for instances assigned to `node_name` and the remote
/// node's self-reported `container_status` for the rest, returning as soon as
/// each is confirmed down. An instance with no report yet is treated as still
/// running. Errors naming the instances still unconfirmed at `deadline`; a
/// container docker cannot be asked about counts as unconfirmed.
async fn wait_for_stopped(
    cluster: &Arc<ClusterNode>,
    node_name: &str,
    instances: &[InstanceId],
    docker: &cl_docker::DockerHandle,
    deadline: Duration,
) -> Result<()> {
    let start = tokio::time::Instant::now();
    loop {
        let state = cluster.state().await;
        let local: Vec<String> = state
            .assignments
            .values()
            .filter(|a| a.node == node_name && instances.contains(&a.instance))
            .map(|a| a.instance.container_name())
            .collect();

        let mut not_stopped: Vec<String> = Vec::new();
        match docker.get().await {
            Some(docker) => {
                for name in &local {
                    match docker.inspect(name).await {
                        Ok(Some(s)) if s.running => not_stopped.push(name.clone()),
                        Ok(_) => {}
                        Err(_) => not_stopped.push(name.clone()),
                    }
                }
            }
            None => not_stopped.extend(local.iter().cloned()),
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
        tokio::time::sleep(Duration::from_secs(1).min(deadline - elapsed)).await;
    }
}

/// Whether every volume on `drive` is caught up to its primary's generation on
/// `node`. Served drives have no replica lag.
fn drive_synced(registry: &DriveRegistry, state: &ClusterState, drive: &str, node: &str) -> bool {
    if registry.is_served(drive) {
        return true;
    }
    state
        .volumes
        .iter()
        .filter(|(id, _)| id.drive == drive)
        .all(|(_, v)| v.in_sync().contains(node))
}

/// Poll until `drive` is fully synced on `node`, or `deadline` elapses. A primary's
/// own entry is bumped on write, so the common case returns immediately.
async fn wait_for_drive_synced(
    cluster: &Arc<ClusterNode>,
    registry: &DriveRegistry,
    drive: &str,
    node: &str,
    deadline: Duration,
) {
    let start = tokio::time::Instant::now();
    loop {
        let elapsed = start.elapsed();
        let state = cluster.state().await;
        if drive_synced(registry, &state, drive, node) || elapsed >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_secs(5).min(deadline - elapsed)).await;
    }
}

/// Whether this node holds the authoritative copy of `drive`: the active server
/// of a served drive, or primary for every volume of a replicated one (a drive
/// whose volumes disagree on primary has no single source and is skipped).
fn is_local_source(registry: &DriveRegistry, state: &ClusterState, drive: &str) -> bool {
    if !registry.implements(drive) {
        return false;
    }
    if registry.is_served(drive) {
        return registry.active_server(drive, state) == Some(registry.node());
    }
    let mut volumes = state
        .volumes
        .iter()
        .filter(|(id, _)| id.drive == drive)
        .peekable();
    if volumes.peek().is_none() {
        return false;
    }
    volumes.all(|(_, v)| v.primary == registry.node())
}

/// Whether this node is the one to push `id` to a backup staging area: the
/// active server of a served drive, or the volume's own primary on a
/// replicated one. A replicated drive's volumes can have different primaries,
/// so each node pushes only the volumes it leads - requiring one node to lead
/// them all (`is_local_source`) meant such a drive was never pushed at all and
/// the destination kept backing up its first, stale staged copy.
fn pushes_volume(
    registry: &DriveRegistry,
    state: &ClusterState,
    id: &cl_raft::ids::VolumeId,
    volume: &cl_raft::state::VolumeState,
) -> bool {
    if !registry.implements(&id.drive) {
        return false;
    }
    if registry.is_served(&id.drive) {
        return registry.active_server(&id.drive, state) == Some(registry.node());
    }
    volume.primary == registry.node()
}

/// Whether this node pushes at least one volume of `drive`.
fn pushes_volumes(registry: &DriveRegistry, state: &ClusterState, drive: &str) -> bool {
    state
        .volumes
        .iter()
        .any(|(id, v)| id.drive == drive && pushes_volume(registry, state, id, v))
}

/// Pushes every volume of `drive` this node leads to `target_node`'s copy of the
/// drive, once. Never followed by `SetVolumeReplica`: the target is a staging
/// area, not a replica, so replication bookkeeping must not learn of it (see
/// `folder_import::push_now`, `cl_drive::migration::sync_all_inner`).
async fn push_drive_once(
    state: &ClusterState,
    registry: &DriveRegistry,
    drive: &str,
    target_node: &str,
    destination_path: &str,
    keys: &cl_raft::KeyWatch,
) -> Result<()> {
    let addr = state
        .nodes
        .get(target_node)
        .map(|ns| ns.rpc_addr)
        .ok_or_else(|| anyhow!("node {target_node} is not in the cluster"))?;
    let peer = SocketAddr::new(addr.ip(), drive_port(addr.port()));
    let engine = DeltaEngine::new(keys.clone());
    for (id, vstate) in state
        .volumes
        .iter()
        .filter(|(id, v)| id.drive == drive && pushes_volume(registry, state, id, v))
    {
        let src = registry.volume_path(id)?;
        engine
            .sync(
                &src,
                peer,
                id,
                vstate.generation,
                Some(destination_path.to_string()),
                ProgressSink::none(),
            )
            .await
            .map_err(|e| anyhow!("syncing {id} to {target_node}: {e}"))?;
    }
    Ok(())
}

/// How often live progress is written back to raft; throttled so restic's
/// several-per-second status lines do not flood raft proposals.
const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_secs(3);

#[allow(clippy::too_many_arguments)]
async fn run_restic_backup(
    cluster: &Arc<ClusterNode>,
    key: &ClusterKey,
    data_dir: &Path,
    state: &ClusterState,
    run: &BackupRun,
    target: &DriveBackupTarget,
    destination: &BackupDestination,
    source_path: &Path,
) -> Result<BackupRunOutcome> {
    backup_secrets::ensure_password_secret(cluster, key, state, &destination.password_secret)
        .await?;
    let state = cluster.state().await;
    let cred_dir = data_dir.join("restic").join("creds").join(&destination.id);
    let (repo, _guards) = backup_secrets::build_repo(&state, key, &cred_dir, destination)?;

    let tag = target.drive.clone();
    let forget = forget_args(target);
    let source_path = source_path.to_path_buf();

    // Percent updates flow off the blocking thread via this watch channel; the
    // task forwards each to raft as an in-place update of the same run id.
    let (progress_tx, mut progress_rx) = tokio::sync::watch::channel::<Option<f64>>(None);
    let run_for_progress = run.clone();
    let cluster_for_progress = Arc::clone(cluster);
    let reporter = tokio::spawn(async move {
        while progress_rx.changed().await.is_ok() {
            let percent = *progress_rx.borrow();
            let mut updated = run_for_progress.clone();
            updated.outcome = BackupRunOutcome::Running { percent };
            let _ = cluster_for_progress
                .submit(Request::RecordBackupRun(updated))
                .await;
        }
    });

    // restic blocks: never call it on an async stack (`clouduler-drive-sync-blocks-runtime`).
    let result = tokio::task::spawn_blocking(move || -> Result<BackupRunOutcome> {
        repo.ensure_initialized()
            .map_err(|e| anyhow!("restic repository: {e}"))?;
        let mut last_sent = std::time::Instant::now()
            .checked_sub(PROGRESS_REPORT_INTERVAL)
            .unwrap_or_else(std::time::Instant::now);
        let outcome = repo
            .backup(&[source_path.as_path()], &[tag.as_str()], |percent| {
                let now = std::time::Instant::now();
                if now.duration_since(last_sent) >= PROGRESS_REPORT_INTERVAL {
                    last_sent = now;
                    let _ = progress_tx.send(Some(percent));
                }
            })
            .map_err(|e| anyhow!("restic backup: {e}"))?;
        // A prune/forget error must not invalidate a good snapshot, but is recorded
        // on the run rather than only logged: until it succeeds the repo grows unbounded.
        let warning = match repo.forget(&forget) {
            Ok(()) => None,
            Err(e) => {
                warn!("restic forget/prune warning (snapshot was preserved): {e}");
                Some(format!(
                    "snapshot saved, but applying the retention policy (forget --prune) failed: {e}"
                ))
            }
        };
        Ok(BackupRunOutcome::Succeeded {
            snapshot_id: outcome.snapshot_id,
            bytes: outcome.bytes,
            warning,
        })
    })
    .await
    .map_err(|e| anyhow!("the restic task panicked: {e}"))?;

    let _ = reporter.await;
    result
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {

    fn led_by(id: &cl_raft::ids::VolumeId, primary: &str) -> cl_raft::state::VolumeState {
        cl_raft::state::VolumeState {
            id: id.clone(),
            generation: 1,
            primary: primary.into(),
            replicas: Default::default(),
            frozen: None,
            sync_refused: None,
            looks_empty: false,
        }
    }

    #[test]
    fn each_node_pushes_only_the_volumes_it_leads() {
        use std::collections::BTreeMap;
        let registry = DriveRegistry::from_drives(
            "a",
            BTreeMap::from([("fast".to_string(), PathBuf::from("/x"))]),
        );
        let mut state = ClusterState::default();
        let mine = cl_raft::ids::VolumeId::new("fast", "one");
        let theirs = cl_raft::ids::VolumeId::new("fast", "two");
        state.volumes.insert(mine.clone(), led_by(&mine, "a"));
        state.volumes.insert(theirs.clone(), led_by(&theirs, "b"));
        assert!(pushes_volume(
            &registry,
            &state,
            &mine,
            &state.volumes[&mine]
        ));
        assert!(!pushes_volume(
            &registry,
            &state,
            &theirs,
            &state.volumes[&theirs]
        ));
        // Leading only some of a drive's volumes still makes this node a pusher.
        assert!(pushes_volumes(&registry, &state, "fast"));
        assert!(!is_local_source(&registry, &state, "fast"));
    }
    use super::*;
    use cl_raft::ids::InstanceId;
    use cl_raft::state::{Assignment, DesiredState, StackState};

    fn stack_with_volume(drive: &str, volume: &str, stop_before: Option<bool>) -> StackState {
        let stop_line = match stop_before {
            Some(true) => "\n        backup:\n          stop_before: true",
            Some(false) => "\n        backup:\n          stop_before: false",
            None => "",
        };
        StackState {
            name: "app".into(),
            spec: format!(
                "services:\n  web:\n    image: nginx\n    volumes:\n      - {drive}/{volume}:/data\n    x-clouduler:{stop_line}\n"
            ),
            revision: 1,
            configs: Default::default(),
            secrets: Default::default(),
            used_configs: Default::default(),
            used_secrets: Default::default(),
            deployed_at: 0,  previous_spec: None,
        }
    }

    fn assignment(instance: InstanceId, node: &str) -> Assignment {
        Assignment {
            instance,
            node: node.into(),
            desired: DesiredState::Running,
            image_digest: None,
            migration: None,
        }
    }

    fn target(stop_containers: Option<bool>) -> DriveBackupTarget {
        DriveBackupTarget {
            id: "t1".into(),
            drive: "fast".into(),
            destination_id: "d1".into(),
            enabled: true,
            frequency_secs: 86_400,
            time_of_day: None,
            keep_last: None,
            keep_within_secs: None,
            keep_daily: None,
            keep_weekly: None,
            keep_monthly: None,
            keep_yearly: None,
            stop_containers,
            last_run: None,
        }
    }

    #[test]
    fn instances_using_drive_finds_every_service_mounting_it() {
        let mut state = ClusterState::default();
        state
            .stacks
            .insert("app".into(), stack_with_volume("fast", "data", None));
        let instance = InstanceId::new("app", "web", 0);
        state
            .assignments
            .insert(instance.clone(), assignment(instance.clone(), "n1"));

        assert_eq!(instances_using_drive(&state, "fast"), vec![instance]);
        assert!(instances_using_drive(&state, "other").is_empty());
    }

    #[test]
    fn target_override_wins_regardless_of_stack_overrides() {
        let mut state = ClusterState::default();
        state
            .stacks
            .insert("app".into(), stack_with_volume("fast", "data", Some(false)));
        let instance = InstanceId::new("app", "web", 0);
        state
            .assignments
            .insert(instance.clone(), assignment(instance.clone(), "n1"));

        assert!(resolve_stop_containers(
            &target(Some(true)),
            &state,
            &[instance]
        ));
    }

    #[test]
    fn no_explicit_answer_defaults_to_stopping() {
        let state = ClusterState::default();
        assert!(resolve_stop_containers(&target(None), &state, &[]));
    }

    #[test]
    fn unanimous_false_skips_stopping() {
        let mut state = ClusterState::default();
        state
            .stacks
            .insert("app".into(), stack_with_volume("fast", "data", Some(false)));
        let instance = InstanceId::new("app", "web", 0);
        state
            .assignments
            .insert(instance.clone(), assignment(instance.clone(), "n1"));

        assert!(!resolve_stop_containers(&target(None), &state, &[instance]));
    }

    #[test]
    fn any_explicit_true_wins_over_a_false() {
        let mut state = ClusterState::default();
        state
            .stacks
            .insert("a".into(), stack_with_volume("fast", "x", Some(false)));
        state
            .stacks
            .insert("b".into(), stack_with_volume("fast", "y", Some(true)));
        let i1 = InstanceId::new("a", "web", 0);
        let i2 = InstanceId::new("b", "web", 0);
        state
            .assignments
            .insert(i1.clone(), assignment(i1.clone(), "n1"));
        state
            .assignments
            .insert(i2.clone(), assignment(i2.clone(), "n1"));

        assert!(resolve_stop_containers(&target(None), &state, &[i1, i2]));
    }

    fn running(id: &str, percent: Option<f64>) -> BackupRun {
        BackupRun {
            id: id.into(),
            target_id: "t1".into(),
            drive: "fast".into(),
            destination_id: "d1".into(),
            started_at: 0,
            finished_at: None,
            outcome: BackupRunOutcome::Running { percent },
            triggered_by: BackupTrigger::Schedule,
        }
    }

    #[test]
    fn a_frequent_target_is_not_condemned_after_a_few_multiples_of_its_period() {
        // 5 minutes * 3 would be 15 minutes - far too short for a big first backup.
        assert_eq!(stale_after_secs(Some(300)), STALE_RUN_FLOOR_SECS);
        assert_eq!(stale_after_secs(Some(86_400)), 72 * 3600);
        assert_eq!(stale_after_secs(Some(86_400 * 30)), STALE_RUN_CAP_SECS);
        assert_eq!(stale_after_secs(None), STALE_RUN_CAP_SECS);
        // Garbage frequencies cannot overflow.
        assert_eq!(stale_after_secs(Some(u64::MAX)), STALE_RUN_CAP_SECS);
    }

    #[test]
    fn progress_resets_the_staleness_clock() {
        let mut tracker = ProgressTracker::default();
        // First sight (e.g. right after a leader change) is credited as progress.
        assert_eq!(tracker.last_progress(&running("r", None), 1_000), 1_000);
        // No change: the clock does not move.
        assert_eq!(tracker.last_progress(&running("r", None), 5_000), 1_000);
        // Percent moved: it restarts.
        assert_eq!(
            tracker.last_progress(&running("r", Some(0.1)), 6_000),
            6_000
        );
        assert_eq!(
            tracker.last_progress(&running("r", Some(0.1)), 9_000),
            6_000
        );
        assert_eq!(
            tracker.last_progress(&running("r", Some(0.2)), 9_500),
            9_500
        );
    }

    #[test]
    fn only_instances_that_want_to_run_are_restarted_later() {
        let mut state = ClusterState::default();
        let up = InstanceId::new("app", "web", 0);
        let down = InstanceId::new("app", "web", 1);
        state
            .assignments
            .insert(up.clone(), assignment(up.clone(), "n1"));
        let mut already_stopped = assignment(down.clone(), "n1");
        already_stopped.desired = DesiredState::Stopped;
        state.assignments.insert(down.clone(), already_stopped);

        assert_eq!(
            currently_running(&state, &[up.clone(), down.clone()]),
            vec![up]
        );
        assert!(currently_running(&state, &[down]).is_empty());
    }

    fn succeeded() -> BackupRunOutcome {
        BackupRunOutcome::Succeeded {
            snapshot_id: "abc".into(),
            bytes: 1,
            warning: None,
        }
    }

    #[test]
    fn restart_trouble_never_hides_the_backups_own_error() {
        let err = combine_with_restart(Err(anyhow!("restic exploded")), Err("raft down".into()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("restic exploded") && err.contains("raft down"),
            "{err}"
        );
        let err = combine_with_restart(Err(anyhow!("restic exploded")), Ok(()))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "restic exploded");
    }

    #[test]
    fn a_failed_restart_after_a_good_snapshot_becomes_a_warning_not_a_failure() {
        match combine_with_restart(Ok(succeeded()), Err("raft down".into())).unwrap() {
            BackupRunOutcome::Succeeded { warning, .. } => {
                assert!(warning.unwrap().contains("raft down"))
            }
            other => panic!("expected success, got {other:?}"),
        }
        assert_eq!(
            combine_with_restart(Ok(succeeded()), Ok(())).unwrap(),
            succeeded()
        );
    }
}
