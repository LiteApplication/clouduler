//! Drives `restic_targets` through to `restic_runs`: decide what's due
//! (leader-side), then, on whichever node(s) actually hold the data, stop
//! affected instances if configured to, get the bytes to the destination,
//! run restic, and restart.
//!
//! Two loops, matching `watch_git_backup` / `crate::migration::start`'s
//! split between "decide" and "do":
//!
//! - [`start_due_checker`] runs only on the leader (a single writer avoids
//!   every manager racing to start the same run) and turns an `is_due`
//!   target into a `Running` `BackupRun`, plus fails any `Running` run that
//!   has sat unclaimed for too long - the crash/partition safety net, since
//!   nothing here is phase-recorded the way `cl_drive::migration` is: restic's
//!   own repository lock already makes a retried run safe, and there is no
//!   point-of-no-return step like migration's `Switching` to protect.
//! - [`start_runner`] runs on every node and claims the `Running` runs that
//!   are its job to execute - see `is_local_source` and the `NodeLocal`
//!   handling below for what "its job" means.
//!
//! ## The `NodeLocal`-elsewhere handoff
//!
//! restic always runs as a host process against a real local path, so a
//! `NodeLocal` destination on a node that doesn't already hold the drive
//! needs the data pushed there first (see `push_drive_once`, using the same
//! one-off `SyncEngine::sync` call `cl_drive::migration` makes - deliberately
//! never followed by `SetVolumeReplica`, since this is a point-in-time copy
//! for restic to read, not a standing replica). `SyncEngine::sync` only ever
//! pushes from the caller outward, so the source node and the destination
//! node are necessarily two different nodes' loops cooperating with no
//! direct channel between them beyond the shared `Running` run: the source
//! pushes once (best effort, guarded in-memory so a slow push isn't retried
//! every tick) and the destination, once it is past a fixed grace period
//! from `started_at`, just runs restic against whatever has landed. This is
//! a deliberate simplification - a real "sync is done" signal would need a
//! new cross-node channel this feature doesn't otherwise need - and a push
//! that is still in flight when the grace period elapses just means that
//! occurrence's restic run reads a partial tree; the next scheduled run
//! (or the stale-run watchdog, for a source that never got there at all)
//! corrects it.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use cl_config::StackFile;
use cl_docker::Docker;
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

/// How often the leader checks for due targets and stale runs.
const DUE_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// How often every node checks for `Running` runs that are its job.
const RUN_POLL_INTERVAL: Duration = Duration::from_secs(15);
/// A `Running` run older than `frequency_secs * this`, capped below, is
/// presumed abandoned (the node that claimed it crashed or partitioned) and
/// failed so the target is eligible to be picked up again on schedule.
const STALE_RUN_MULTIPLIER: i64 = 3;
const STALE_RUN_CAP_SECS: i64 = 72 * 3600;
/// How long a `NodeLocal`-elsewhere destination waits, once a run starts,
/// before running restic against whatever the source's one-off push has
/// landed so far - see the module doc comment.
const SYNC_GRACE_SECS: i64 = 120;
/// How long a local, non-primary copy of a drive is given to catch up with
/// its primary before a backup runs against it anyway.
const DRIVE_SYNC_GRACE_SECS: i64 = 120;
/// On top of the affected services' own `stop_grace_period`, how long to
/// wait for local containers to actually stop before giving up and running
/// the backup anyway - mirrors `folder_import`'s margin.
const STOP_POLL_MARGIN: Duration = Duration::from_secs(15);

/// Leader-side: turn due targets into `Running` runs, and fail stale ones.
pub fn start_due_checker(cluster: Arc<ClusterNode>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(DUE_POLL_INTERVAL);
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
                // A manual run (or a previous scheduled one that hasn't
                // finished yet) already owns this target; the state machine
                // would reject a second `Running` run for it anyway, but
                // skipping here avoids a pointless submit every poll while
                // it's in flight.
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

            for run in state.restic_runs.values() {
                if !matches!(run.outcome, BackupRunOutcome::Running { .. }) {
                    continue;
                }
                let max_age = state
                    .restic_targets
                    .get(&run.target_id)
                    .map(|t| {
                        (t.frequency_secs as i64 * STALE_RUN_MULTIPLIER).min(STALE_RUN_CAP_SECS)
                    })
                    .unwrap_or(STALE_RUN_CAP_SECS);
                if now - run.started_at >= max_age {
                    let mut stale = run.clone();
                    stale.finished_at = Some(now);
                    stale.outcome = BackupRunOutcome::Failed {
                        error: "interrupted: no node finished this run in time".to_string(),
                    };
                    let _ = cluster.submit(Request::RecordBackupRun(stale)).await;
                }
            }
        }
    })
}

/// Per-node: claim and execute the `Running` runs that are this node's job.
pub fn start_runner(
    cluster: Arc<ClusterNode>,
    node_name: String,
    registry: Arc<tokio::sync::RwLock<Option<Arc<DriveRegistry>>>>,
    docker: Arc<tokio::sync::RwLock<Option<Docker>>>,
    key_rx: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
    data_dir: PathBuf,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(RUN_POLL_INTERVAL);
        // Runs this node has already claimed (`execute_and_finish` is in
        // flight for them) or already pushed once as a `NodeLocal`-elsewhere
        // source - both reset on restart, which is harmless: a retried
        // execute is safe (restic's repo lock) and a retried push is cheap
        // (delta-based).
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
                // No blanket `registry.implements(&run.drive)` gate here:
                // `is_source` below already requires it for every branch
                // that needs to read the drive, but the `NodeLocal { node
                // == node_name }` destination branch must run even when
                // this node does *not* implement the drive - that's exactly
                // the `NodeLocal`-elsewhere case (see the module doc
                // comment), and gating on it here used to skip that branch
                // entirely, leaving those runs stuck `Running` forever.
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
                        if is_source && node != &node_name =>
                    {
                        if !pushed.lock().await.insert(run.id.clone()) {
                            continue;
                        }
                        if let Err(e) =
                            push_drive_once(&state, &registry, &run.drive, node, path).await
                        {
                            warn!(run = %run.id, drive = %run.drive, destination = %node, error = %e, "one-off backup sync failed");
                            // The destination can't run restic against data
                            // that never landed, and it can't tell a push
                            // that's still in flight from one that already
                            // failed outright - only the source knows that.
                            // Fail the run itself rather than leaving it
                            // `Running` until the stale-run watchdog gets to
                            // it, which can be up to `STALE_RUN_CAP_SECS`
                            // later.
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
    docker: &Arc<tokio::sync::RwLock<Option<Docker>>>,
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
    docker: &Arc<tokio::sync::RwLock<Option<Docker>>>,
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

    let mut stopped: Vec<InstanceId> = Vec::new();
    if should_stop && !instances.is_empty() {
        // Stopped one at a time - each instance's own deadline elapses
        // before the next is even asked to stop - rather than firing every
        // `SetDesiredState` at once and waiting on the slowest, so a single
        // node doesn't have to shut down every affected container in the
        // same instant.
        for instance in &instances {
            let one = std::slice::from_ref(instance);
            set_desired(cluster, one, DesiredState::Stopped).await?;
            let deadline = max_stop_grace(state, one) + STOP_POLL_MARGIN;
            wait_for_stopped(cluster, node_name, one, docker, deadline).await;
        }
        stopped = instances;
    }

    // A `NodeLocal`-elsewhere destination doesn't implement the drive at
    // all - see the module doc comment - so it has no `registry.root` to
    // read; restic runs against the staging copy `push_drive_once`'s peer
    // landed instead, in a hidden directory next to the destination's own
    // path rather than under this node's data dir (see
    // `DriveRegistry::staging_root_for`) - the staged copy can be the
    // whole drive, and the destination's own volume is the one an operator
    // actually sized for that. `ensure_staging_root` (rather than just
    // `staging_root_for`) matters here: a run whose grace period has
    // elapsed with no push having landed yet (source unreachable, say)
    // still needs a real, existing directory for restic to point at.
    let source_path = if registry.implements(&run.drive) {
        // A local copy of a replicated drive can lag its primary (a fresh
        // replica, or one that fell behind) - back it up straight off disk
        // without checking and restic would capture stale data with no
        // warning. Give it a bounded chance to catch up first; a copy still
        // behind once the deadline passes is backed up as-is, same trade-off
        // `SYNC_GRACE_SECS` accepts for the `NodeLocal`-elsewhere case - the
        // next scheduled run corrects it.
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
    let result = run_restic_backup(
        cluster,
        key,
        data_dir,
        state,
        run,
        target,
        destination,
        &source_path,
    )
    .await;

    if !stopped.is_empty() {
        set_desired(cluster, &stopped, DesiredState::Running).await?;
    }
    result
}

/// Instances whose stack mounts a volume on `drive` - `folder_import`'s
/// `instances_using` generalized from one volume to every volume on a drive,
/// since a backup run covers the whole drive in one restic call.
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

/// The target's own override wins outright; otherwise, most-conservative
/// wins across every affected service's resolved `x-clouduler` override
/// (`effective_backup_stop_before`): any explicit `true` stops, and only a
/// unanimous explicit `false` skips stopping. No explicit answer from
/// anyone defaults to `true` - stopping before touching live data is the
/// safe default.
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
    instances
        .iter()
        .filter_map(|i| {
            let stack = state.stacks.get(&i.stack)?;
            let file = StackFile::parse(&stack.spec).ok()?;
            file.services.get(&i.service)?.stop_grace_period
        })
        .max()
        .unwrap_or(cl_sched::reconcile::DEFAULT_STOP_GRACE)
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

/// Polls local docker for instances assigned to `node_name`; for anything
/// assigned elsewhere, polls that node's self-reported `container_status`
/// (see `cl_sched::run::Scheduler::report_container_status`) instead of
/// blindly sleeping out `deadline` - so this returns the moment the remote
/// node actually confirms its container is down, rather than always waiting
/// the worst case. An instance with no report yet (an older peer, or one
/// that hasn't swept since this run started) is treated as still running,
/// falling back to the old blind wait for exactly that instance.
async fn wait_for_stopped(
    cluster: &Arc<ClusterNode>,
    node_name: &str,
    instances: &[InstanceId],
    docker: &Arc<tokio::sync::RwLock<Option<Docker>>>,
    deadline: Duration,
) {
    let start = tokio::time::Instant::now();
    loop {
        let elapsed = start.elapsed();
        if elapsed >= deadline {
            return;
        }
        let state = cluster.state().await;
        let local: Vec<String> = state
            .assignments
            .values()
            .filter(|a| a.node == node_name && instances.contains(&a.instance))
            .map(|a| a.instance.container_name())
            .collect();

        let mut all_stopped = true;
        if let Some(docker) = docker.read().await.clone() {
            for name in &local {
                match docker.inspect(name).await {
                    Ok(Some(s)) if s.running => all_stopped = false,
                    _ => {}
                }
            }
        } else if !local.is_empty() {
            all_stopped = false;
        }

        for instance in instances {
            if local.contains(&instance.container_name()) {
                continue;
            }
            match state.container_status.get(instance) {
                Some(status) if status.running => all_stopped = false,
                Some(_) => {}
                None => all_stopped = false,
            }
        }

        if all_stopped {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1).min(deadline - elapsed)).await;
    }
}

/// Whether every volume on `drive` is caught up to its primary's generation
/// on `node`. Served drives have no replica lag to check - only the active
/// server ever holds a copy, so it's trivially synced.
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

/// Poll until `drive` is fully synced on `node`, or `deadline` elapses. A
/// primary's own replica entry is bumped the instant it writes (see
/// `TouchVolume`), so this returns immediately for the common case and only
/// actually waits for a replica that is genuinely behind.
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

/// Whether this node currently holds the authoritative copy of `drive`: for
/// a served drive, whether it's the active server; for a replicated drive,
/// whether it is primary for every volume on record (a drive whose volumes
/// disagree on primary has no single source and is skipped - see the module
/// doc comment on the `NodeLocal`-elsewhere handoff for why a single source
/// is needed at all).
fn is_local_source(registry: &DriveRegistry, state: &ClusterState, drive: &str) -> bool {
    if !registry.implements(drive) {
        return false;
    }
    if registry.is_served(drive) {
        return registry.active_server(drive, &state.unreachable) == Some(registry.node());
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

/// Pushes every volume of `drive` to `target_node`'s own copy of the same
/// drive, once. Never followed by `SetVolumeReplica`: `target_node` is a
/// backup destination's staging area, not a standing replica, so the
/// cluster's replication bookkeeping must never learn about this write - see
/// `folder_import::push_now` for the pattern this mirrors and
/// `cl_drive::migration::sync_all_inner` for the call shape.
async fn push_drive_once(
    state: &ClusterState,
    registry: &DriveRegistry,
    drive: &str,
    target_node: &str,
    destination_path: &str,
) -> Result<()> {
    let addr = state
        .nodes
        .get(target_node)
        .map(|ns| ns.rpc_addr)
        .ok_or_else(|| anyhow!("node {target_node} is not in the cluster"))?;
    let peer = SocketAddr::new(addr.ip(), drive_port(addr.port()));
    let engine = DeltaEngine::default();
    for (id, vstate) in state.volumes.iter().filter(|(id, _)| id.drive == drive) {
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

/// How often live progress is written back to raft while a backup runs -
/// throttled here (rather than on every restic status line, which can be
/// several a second) so a long backup doesn't turn into a flood of raft
/// proposals; `run_restic_backup`'s `on_progress` closure only forwards a
/// value once this much time has passed since the last one it sent.
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
    // Re-read: `ensure_password_secret` may just have written the entry
    // `build_repo` needs to open.
    let state = cluster.state().await;
    let cred_dir = data_dir.join("restic").join("creds").join(&destination.id);
    let (repo, _guards) = backup_secrets::build_repo(&state, key, &cred_dir, destination)?;

    let tag = target.drive.clone();
    let forget = forget_args(target);
    let source_path = source_path.to_path_buf();

    // Percent updates flow off the blocking thread through this watch
    // channel; the task below forwards each one to raft (via the same run
    // id, so it's an in-place update, not a new run - see the state
    // machine's `RecordBackupRun` handling) and stops once the channel
    // closes, which happens the instant the blocking closure returns.
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

    // restic is a blocking `std::process::Command` - never call it straight
    // off an async fn's stack, see `clouduler-drive-sync-blocks-runtime`.
    let result = tokio::task::spawn_blocking(move || -> Result<BackupRunOutcome> {
        if !repo.is_initialized() {
            repo.init().map_err(|e| anyhow!("restic init: {e}"))?;
        }
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
        if let Err(e) = repo.forget(&forget) {
            // A prune/forget error (e.g. transient lock or network issue) shouldn't
            // invalidate a successful snapshot of large data.
            warn!("restic forget/prune warning (snapshot was preserved): {e}");
        }
        Ok(BackupRunOutcome::Succeeded {
            snapshot_id: outcome.snapshot_id,
            bytes: outcome.bytes,
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
}
