//! The sync scheduler: keeps replicas of every volume this node is primary
//! for moving towards the other nodes that implement the same drive.
//!
//! One task per volume. Each tick it checks quorum and whether the volume's
//! interval (with jitter) has elapsed, then runs one engine sync per replica,
//! reporting the generation reached through raft. A per-volume guard stops a
//! slow sync overlapping its next tick.
//!
//! A served drive's volumes use the same loop with a different source and
//! targets: the source is `DriveRegistry::active_server`, not
//! `VolumeState.primary` (the node an instance runs on, not the data holder),
//! and the targets are the other `served_by` candidates, since a plain
//! mounting node has no local copy.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{debug, warn};

use cl_raft::{ClusterNode, QuorumWatch, Request, VolumeId};

use crate::delta::{BoxFuture, SyncEngine};
use crate::registry::DriveRegistry;

/// The volume's own interval override, else the drive default.
pub fn resolve_interval(drive_default: Duration, volume_override: Option<Duration>) -> Duration {
    volume_override.unwrap_or(drive_default)
}

/// Spread ticks across the interval so volumes don't line up. `spread` in
/// `[0, 1)` picks the tick position (random at runtime, fixed in tests).
pub fn jitter(interval: Duration, spread: f64) -> Duration {
    let spread = spread.clamp(0.0, 0.999);
    Duration::from_secs_f64(interval.as_secs_f64() * spread)
}

/// Tracks volumes with a sync in flight, so none is started twice.
#[derive(Default)]
pub struct InFlightGuard {
    running: Mutex<BTreeSet<VolumeId>>,
}

impl InFlightGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` if this call claimed the volume; `false` if already running (skip the tick).
    pub async fn try_start(&self, volume: &VolumeId) -> bool {
        self.running.lock().await.insert(volume.clone())
    }

    pub async fn finish(&self, volume: &VolumeId) {
        self.running.lock().await.remove(volume);
    }

    pub async fn is_running(&self, volume: &VolumeId) -> bool {
        self.running.lock().await.contains(volume)
    }
}

#[derive(Debug, Clone)]
pub struct ReplicaTarget {
    pub node: String,
    pub addr: std::net::SocketAddr,
}

/// Supplies the per-volume interval; the daemon derives it from drive defaults
/// and each stack's `x-clouduler.volume_sync`. `state` is this pass's replicated
/// state.
pub trait IntervalSource: Send + Sync {
    fn interval_for(&self, volume: &VolumeId, state: &cl_raft::ClusterState) -> Duration;

    /// Best (lowest) `priority` of any stack using the drive. Volumes due in the
    /// same pass sync in ascending order of this.
    fn priority_for(&self, _volume: &VolumeId, _state: &cl_raft::ClusterState) -> i32 {
        0
    }

    /// How many of `drive`'s volumes may replicate at once from this node.
    fn parallelism_for(&self, _drive: &str) -> usize {
        1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPhase {
    Before,
    After,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// Nothing configured for this volume and phase.
    None,
    Ok,
    Failed(String),
}

/// Commands run around a volume's replication, supplied by the daemon (which
/// knows the stacks and docker).
pub trait SyncHooks: Send + Sync {
    fn run<'a>(&'a self, volume: &'a VolumeId, phase: SyncPhase) -> BoxFuture<'a, HookOutcome>;
}

pub struct SyncScheduler<E: SyncEngine> {
    node: Arc<ClusterNode>,
    registry: Arc<DriveRegistry>,
    engine: Arc<E>,
    quorum: QuorumWatch,
    hooks: Option<Arc<dyn SyncHooks>>,
    guard: Arc<InFlightGuard>,
    /// Volumes found without a file on the previous pass; one empty reading is
    /// not enough (a file can be mid-replace).
    empty_seen: parking_lot::Mutex<BTreeSet<VolumeId>>,
    /// How often to check whether any volume is due.
    tick: Duration,
}

impl<E: SyncEngine + 'static> SyncScheduler<E> {
    pub fn new(node: Arc<ClusterNode>, registry: Arc<DriveRegistry>, engine: Arc<E>) -> Self {
        let quorum = node.quorum();
        Self {
            node,
            registry,
            engine,
            quorum,
            hooks: None,
            guard: Arc::new(InFlightGuard::new()),
            empty_seen: parking_lot::Mutex::default(),
            tick: Duration::from_secs(30),
        }
    }

    pub fn with_hooks(mut self, hooks: Arc<dyn SyncHooks>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    async fn hook(&self, id: &VolumeId, phase: SyncPhase) -> HookOutcome {
        let Some(hooks) = &self.hooks else {
            return HookOutcome::None;
        };
        let outcome = hooks.run(id, phase).await;
        if let HookOutcome::Failed(reason) = &outcome {
            warn!(%id, ?phase, %reason, "replication hook failed");
        }
        outcome
    }

    /// Run forever; spawn as a background task.
    pub async fn run(&self, intervals: &dyn IntervalSource) {
        let mut next_due: std::collections::BTreeMap<VolumeId, Instant> =
            std::collections::BTreeMap::new();
        let mut ticker = tokio::time::interval(self.tick);
        // Volumes last seen frozen: pushed at once on unfreeze.
        let mut was_frozen: BTreeSet<VolumeId> = BTreeSet::new();

        loop {
            ticker.tick().await;

            if !self.quorum.has_quorum() {
                debug!("no quorum, skipping this sync pass");
                continue;
            }

            let state = self.node.snapshot().await;
            let now = Instant::now();

            self.remove_deleted_volumes(&state.volume_removals).await;

            // Everything due, most important stack first (ties by volume id).
            let mut due: Vec<(i32, VolumeId)> = Vec::new();
            let mut next_interval: std::collections::HashMap<VolumeId, Duration> =
                std::collections::HashMap::new();
            for (id, volume) in &state.volumes {
                if !self.registry.implements(&id.drive) {
                    continue;
                }
                // Frozen for hand edits: nothing syncs out of or over it.
                if volume.frozen.is_some() {
                    was_frozen.insert(id.clone());
                    continue;
                }
                if was_frozen.remove(id) {
                    next_due.insert(id.clone(), now);
                }
                let is_served = self.registry.is_served(&id.drive);
                let is_source = if is_served {
                    self.registry.active_server(&id.drive, &state) == Some(self.registry.node())
                } else {
                    volume.primary == self.registry.node()
                };
                if !is_source {
                    continue;
                }

                let interval = intervals.interval_for(id, &state);
                let due_at = next_due.entry(id.clone()).or_insert_with(|| {
                    let spread: f64 = rand::thread_rng().gen();
                    now + jitter(interval, spread)
                });
                if now < *due_at {
                    continue;
                }
                due.push((intervals.priority_for(id, &state), id.clone()));
                next_interval.insert(id.clone(), interval);
            }
            due.sort();
            // The syncs below can run for minutes: release the shared snapshot first
            // (each batch takes a fresh one) so applies do not deep-copy the state meanwhile.
            drop(state);

            // Most important due volume plus up to `max_parallel_syncs - 1` more of its
            // drive; the next batch starts when this one finishes.
            while !due.is_empty() {
                let width = intervals.parallelism_for(&due[0].1.drive).max(1);
                let batch = take_batch(&mut due, width);
                let fresh = self.node.snapshot().await;
                let pending: Vec<_> = batch
                    .iter()
                    .map(|id| self.sync_due(id, fresh.clone()))
                    .collect();
                drop(fresh);
                let claimed = futures::future::join_all(pending).await;
                for (id, claimed) in batch.into_iter().zip(claimed) {
                    if claimed {
                        if let Some(interval) = next_interval.get(&id) {
                            next_due.insert(id, now + *interval);
                        }
                    }
                }
            }
        }
    }

    /// Claims and replicates one due volume; `false` if already in flight.
    async fn sync_due(&self, id: &VolumeId, state: Arc<cl_raft::ClusterState>) -> bool {
        let Some(volume) = state.volumes.get(id) else {
            return false;
        };
        let (looks_empty, generation) = (volume.looks_empty, volume.generation);
        let is_served = self.registry.is_served(&id.drive);
        if !self.guard.try_start(id).await {
            // Previous run still going; wait for the next tick.
            return false;
        }

        let targets: Vec<ReplicaTarget> = if is_served {
            self.registry
                .served_candidates(&id.drive)
                .iter()
                .filter(|name| name.as_str() != self.registry.node())
                .filter_map(|name| {
                    state.nodes.get(name).map(|n| ReplicaTarget {
                        node: name.clone(),
                        addr: n.rpc_addr,
                    })
                })
                .collect()
        } else {
            state
                .nodes
                .iter()
                .filter(|(name, n)| *name != self.registry.node() && n.implements(&id.drive))
                .map(|(name, n)| ReplicaTarget {
                    node: name.clone(),
                    addr: n.rpc_addr,
                })
                .collect()
        };

        let authority = is_served
            .then(|| state.served.get(&id.drive).cloned())
            .flatten();
        // Release the shared snapshot before the (long) sync.
        drop(state);
        self.check_empty(id, looks_empty).await;
        self.run_one(
            id.clone(),
            generation,
            targets,
            is_served.then_some(authority),
        )
        .await;
        true
    }

    /// Removes this node's copy of every deliberately deleted volume (the cluster
    /// has checked no stack uses it), then says so.
    async fn remove_deleted_volumes(
        &self,
        removals: &std::collections::BTreeMap<VolumeId, BTreeSet<String>>,
    ) {
        for (id, pending) in removals {
            if !pending.contains(self.registry.node()) {
                continue;
            }
            if self.registry.implements(&id.drive) {
                let Ok(path) = self.registry.volume_path(id) else {
                    continue;
                };
                if path.exists() {
                    let target = path.clone();
                    let removed =
                        tokio::task::spawn_blocking(move || std::fs::remove_dir_all(target)).await;
                    match removed {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            warn!(%id, error = %e, "could not delete the volume directory");
                            continue;
                        }
                        Err(e) => {
                            warn!(%id, error = %e, "the volume deletion task failed");
                            continue;
                        }
                    }
                }
            }
            let _ = self
                .node
                .submit(Request::AckVolumeRemoved {
                    id: id.clone(),
                    node: self.registry.node().to_string(),
                })
                .await;
        }
    }

    /// Raises `looks_empty` after two empty passes in a row, lowers it when a file
    /// shows up. Delete-vs-restore is the operator's call.
    async fn check_empty(&self, id: &VolumeId, recorded: bool) {
        let Ok(src) = self.registry.volume_path(id) else {
            return;
        };
        let manifest = match crate::scan::scan_off_runtime(src, false).await {
            Ok(m) => m,
            Err(e) => {
                debug!(%id, error = %e, "could not scan for emptiness");
                return;
            }
        };
        let empty = manifest.files.is_empty();
        let confirmed = {
            let mut seen = self.empty_seen.lock();
            if empty {
                !seen.insert(id.clone())
            } else {
                seen.remove(id);
                false
            }
        };
        if confirmed != recorded && (confirmed || !empty) {
            if let Err(e) = self
                .node
                .submit(Request::SetVolumeEmpty {
                    id: id.clone(),
                    empty: confirmed,
                })
                .await
            {
                warn!(%id, error = %e, "failed to record whether the volume is empty");
            }
        }
    }

    /// Surfaces a refused push as a cluster problem, skipping repeats of the same
    /// refusal to avoid writing raft every pass.
    async fn record_refusal(&self, id: &VolumeId, to: &str, reason: String) {
        let current = self
            .node
            .state()
            .await
            .volumes
            .get(id)
            .and_then(|v| v.sync_refused.clone());
        if current
            .as_ref()
            .is_some_and(|c| c.from == self.registry.node() && c.to == to && c.reason == reason)
        {
            return;
        }
        let refusal = cl_raft::SyncRefusal {
            from: self.registry.node().to_string(),
            to: to.to_string(),
            reason,
            at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64),
        };
        if let Err(e) = self
            .node
            .submit(Request::SetVolumeSyncRefusal {
                id: id.clone(),
                refusal: Some(refusal),
            })
            .await
        {
            warn!(%id, error = %e, "failed to record the refused sync");
        }
    }

    /// Stops replication between two copies that were both written; records the
    /// conflict for an operator.
    async fn record_served_conflict(&self, id: &VolumeId, other: &str, reason: String) {
        warn!(%id, %other, %reason, "served drive copies diverged, keeping both");
        let _ = self
            .node
            .submit(Request::Served {
                drive: id.drive.clone(),
                op: cl_raft::served::ServedOp::Conflict {
                    holder: self.registry.node().to_string(),
                    other: other.to_string(),
                    reason: format!("{id}: {reason}"),
                    at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs() as i64),
                },
            })
            .await;
    }

    async fn clear_refusal(&self, id: &VolumeId) {
        let recorded = self
            .node
            .state()
            .await
            .volumes
            .get(id)
            .is_some_and(|v| v.sync_refused.is_some());
        if recorded {
            let _ = self
                .node
                .submit(Request::SetVolumeSyncRefusal {
                    id: id.clone(),
                    refusal: None,
                })
                .await;
        }
    }

    /// `served` is `Some` for a served drive's volume with its recorded authority:
    /// every target is asked whether its copy was written since it last agreed,
    /// and one that was is never pushed over.
    async fn run_one(
        &self,
        id: VolumeId,
        generation: u64,
        targets: Vec<ReplicaTarget>,
        served: Option<Option<cl_raft::served::ServedAuthority>>,
    ) {
        let src = match self.registry.volume_path(&id) {
            Ok(path) => path,
            Err(e) => {
                warn!(%id, error = %e, "cannot resolve local volume path");
                self.guard.finish(&id).await;
                return;
            }
        };

        // A failed `before` skips replication but `after` still runs: `before` may
        // have paused or locked something halfway.
        let skip = matches!(
            self.hook(&id, SyncPhase::Before).await,
            HookOutcome::Failed(_)
        );

        for target in targets.into_iter().filter(|_| !skip) {
            // Stop pushing the moment a freeze lands mid-run.
            if self
                .node
                .state()
                .await
                .volumes
                .get(&id)
                .is_some_and(|v| v.frozen.is_some())
            {
                debug!(%id, "volume was frozen mid-run, stopping");
                break;
            }
            let drive_addr = crate::transport::drive_port(target.addr.port());
            let peer = std::net::SocketAddr::new(target.addr.ip(), drive_addr);

            let mut synced_digest = None;
            if let Some(authority) = &served {
                if authority
                    .as_ref()
                    .and_then(|a| a.conflict.as_ref())
                    .is_some_and(|c| c.other == target.node || c.holder == target.node)
                {
                    debug!(%id, node = %target.node, "unresolved conflict, not syncing to this node");
                    continue;
                }
                let plan = crate::served::failback::plan_push(
                    self.engine.as_ref(),
                    &self.registry,
                    &id,
                    peer,
                    &target.node,
                    authority.as_ref(),
                )
                .await;
                match plan {
                    Err(e) => {
                        warn!(%id, node = %target.node, error = %e, "could not check the standby's copy, not syncing to it");
                        continue;
                    }
                    Ok(plan) => match plan.direction {
                        crate::served::failback::Direction::Push => {
                            synced_digest = Some(plan.local.current);
                        }
                        crate::served::failback::Direction::TargetAhead => {
                            debug!(%id, node = %target.node, "the standby holds newer data, not syncing to it");
                            continue;
                        }
                        crate::served::failback::Direction::Conflict(reason) => {
                            self.record_served_conflict(&id, &target.node, reason).await;
                            continue;
                        }
                    },
                }
            }

            match self
                .engine
                .sync(
                    &src,
                    peer,
                    &id,
                    generation,
                    None,
                    crate::progress::ProgressSink::none(),
                )
                .await
            {
                Ok(stats) => {
                    debug!(%id, node = %target.node, ?stats, "volume synced");
                    if let Some(digest) = &synced_digest {
                        self.registry.record_synced(&id, digest);
                        if served
                            .as_ref()
                            .and_then(|a| a.as_ref())
                            .is_some_and(|a| a.overwrite.as_deref() == Some(target.node.as_str()))
                        {
                            let _ = self
                                .node
                                .submit(Request::Served {
                                    drive: id.drive.clone(),
                                    op: cl_raft::served::ServedOp::ClearOverwrite,
                                })
                                .await;
                        }
                    }
                    self.clear_refusal(&id).await;
                    if let Err(e) = self
                        .node
                        .submit(Request::SetVolumeReplica {
                            id: id.clone(),
                            node: target.node.clone(),
                            generation,
                        })
                        .await
                    {
                        warn!(%id, node = %target.node, error = %e, "failed to record replica progress");
                    }
                }
                Err(e) => {
                    warn!(%id, node = %target.node, error = %e, "volume sync failed");
                    if let crate::error::DriveError::DestructiveSync { reason, .. } = &e {
                        self.record_refusal(&id, &target.node, reason.clone()).await;
                        // The source is bad for every target; do not try the rest.
                        break;
                    }
                }
            }
        }

        self.hook(&id, SyncPhase::After).await;
        self.guard.finish(&id).await;
    }
}

/// Takes the next batch off the sorted `due` list: its first volume and up to
/// `width - 1` more of the same drive, preserving the order of the rest.
fn take_batch(due: &mut Vec<(i32, VolumeId)>, width: usize) -> Vec<VolumeId> {
    let Some(drive) = due.first().map(|(_, id)| id.drive.clone()) else {
        return Vec::new();
    };
    let mut batch = Vec::new();
    let mut rest = Vec::new();
    for entry in std::mem::take(due) {
        if entry.1.drive == drive && batch.len() < width {
            batch.push(entry.1);
        } else {
            rest.push(entry);
        }
    }
    *due = rest;
    batch
}

/// A static map for callers that already resolved every interval.
pub struct StaticIntervals {
    default: Duration,
    overrides: std::collections::BTreeMap<VolumeId, Duration>,
}

impl StaticIntervals {
    pub fn new(
        default: Duration,
        overrides: std::collections::BTreeMap<VolumeId, Duration>,
    ) -> Self {
        Self { default, overrides }
    }
}

impl IntervalSource for StaticIntervals {
    fn interval_for(&self, volume: &VolumeId, _state: &cl_raft::ClusterState) -> Duration {
        resolve_interval(self.default, self.overrides.get(volume).copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_is_the_first_drive_up_to_the_parallel_limit() {
        let id = |d: &str, v: &str| VolumeId::new(d, v);
        let mut due = vec![
            (-5, id("fast", "a")),
            (0, id("bulk", "x")),
            (1, id("fast", "b")),
            (2, id("fast", "c")),
        ];
        assert_eq!(
            take_batch(&mut due, 2),
            vec![id("fast", "a"), id("fast", "b")]
        );
        assert_eq!(due, vec![(0, id("bulk", "x")), (2, id("fast", "c"))]);
        assert_eq!(take_batch(&mut due, 1), vec![id("bulk", "x")]);
        assert_eq!(take_batch(&mut due, 4), vec![id("fast", "c")]);
        assert!(take_batch(&mut due, 4).is_empty());
    }

    #[test]
    fn interval_falls_back_to_the_drive_default() {
        let default = Duration::from_secs(900);
        assert_eq!(resolve_interval(default, None), default);
    }

    #[test]
    fn interval_override_wins() {
        let default = Duration::from_secs(900);
        let over = Duration::from_secs(60);
        assert_eq!(resolve_interval(default, Some(over)), over);
    }

    #[test]
    fn jitter_stays_within_the_interval() {
        let interval = Duration::from_secs(100);
        assert_eq!(jitter(interval, 0.0), Duration::from_secs(0));
        assert!(jitter(interval, 0.5) < interval);
        assert!(
            jitter(interval, 1.5) < interval,
            "out of range spread is clamped"
        );
    }

    #[test]
    fn static_intervals_prefer_the_override() {
        let state = cl_raft::ClusterState::default();
        let volume = VolumeId::new("fast", "data");
        let intervals = StaticIntervals::new(
            Duration::from_secs(900),
            std::collections::BTreeMap::from([(volume.clone(), Duration::from_secs(30))]),
        );
        assert_eq!(
            intervals.interval_for(&volume, &state),
            Duration::from_secs(30)
        );
        assert_eq!(
            intervals.interval_for(&VolumeId::new("fast", "other"), &state),
            Duration::from_secs(900)
        );
    }

    #[tokio::test]
    async fn in_flight_guard_refuses_a_second_start() {
        let guard = InFlightGuard::new();
        let id = VolumeId::new("fast", "data");
        assert!(guard.try_start(&id).await);
        assert!(
            !guard.try_start(&id).await,
            "second start should be refused"
        );
        assert!(guard.is_running(&id).await);
        guard.finish(&id).await;
        assert!(!guard.is_running(&id).await);
        assert!(
            guard.try_start(&id).await,
            "start allowed again after finish"
        );
    }

    #[tokio::test]
    async fn in_flight_guard_tracks_volumes_independently() {
        let guard = InFlightGuard::new();
        let a = VolumeId::new("fast", "a");
        let b = VolumeId::new("fast", "b");
        assert!(guard.try_start(&a).await);
        assert!(
            guard.try_start(&b).await,
            "a different volume is unaffected"
        );
    }
}
