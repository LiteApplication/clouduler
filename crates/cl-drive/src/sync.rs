//! The sync scheduler: keeps replicas of every volume this node is primary
//! for moving towards the other nodes that implement the same drive.
//!
//! One task runs per volume. Each tick it checks quorum, checks whether the
//! volume's interval has elapsed (with jitter so a fleet of volumes on the
//! same interval doesn't all fire together), and if so runs one engine sync
//! per replica, reporting the generation reached back through raft. A guard
//! keyed by volume id stops two syncs of the same volume overlapping if a
//! slow sync runs past its own next tick.
//!
//! A served drive's volumes go through the same loop, just with a different
//! notion of "am I the source" and "who are the targets": the source is
//! whichever candidate `DriveRegistry::active_server` currently names, not
//! `VolumeState.primary` (which, for a served drive, is the node an
//! instance happens to run on, not the node holding the data), and the
//! targets are the drive's other `served_by` candidates, not every node
//! that implements the drive, since a plain mounting node has no local copy
//! to sync into.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{debug, warn};

use cl_raft::{ClusterNode, QuorumWatch, Request, VolumeId};

use crate::delta::SyncEngine;
use crate::registry::DriveRegistry;

/// The effective sync interval for a volume: its own override if the caller
/// has one, otherwise the drive's default.
pub fn resolve_interval(drive_default: Duration, volume_override: Option<Duration>) -> Duration {
    volume_override.unwrap_or(drive_default)
}

/// Spread ticks across the interval instead of letting every volume line up
/// on the same wall clock second. `spread` in `[0, 1)` picks where in the
/// interval this volume's tick lands; callers pass a random value at
/// runtime and a fixed one in tests.
pub fn jitter(interval: Duration, spread: f64) -> Duration {
    let spread = spread.clamp(0.0, 0.999);
    Duration::from_secs_f64(interval.as_secs_f64() * spread)
}

/// Tracks which volumes currently have a sync in flight, so the scheduler
/// never starts a second one for the same volume while the first is running.
#[derive(Default)]
pub struct InFlightGuard {
    running: Mutex<BTreeSet<VolumeId>>,
}

impl InFlightGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` if this call claimed the volume; `false` if it was already
    /// running and the caller should skip this tick.
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

/// One peer address to replicate a volume to.
#[derive(Debug, Clone)]
pub struct ReplicaTarget {
    pub node: String,
    pub addr: std::net::SocketAddr,
}

/// Supplies the per-volume interval; the daemon derives this from the
/// cluster config's drive defaults and each stack's `x-clouduler.volume_sync`
/// overrides, none of which this crate needs to know how to parse.
pub trait IntervalSource: Send + Sync {
    fn interval_for(&self, volume: &VolumeId) -> Duration;
}

/// Runs the periodic sync loop for one node.
pub struct SyncScheduler<E: SyncEngine> {
    node: Arc<ClusterNode>,
    registry: Arc<DriveRegistry>,
    engine: Arc<E>,
    quorum: QuorumWatch,
    guard: Arc<InFlightGuard>,
    /// Volumes found without a file on the previous pass. One empty reading
    /// is not enough to raise the alarm: a file can be mid-replace.
    empty_seen: std::sync::Mutex<BTreeSet<VolumeId>>,
    /// How often to check whether any volume is due. Independent of any
    /// individual volume's own interval.
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
            guard: Arc::new(InFlightGuard::new()),
            empty_seen: std::sync::Mutex::default(),
            tick: Duration::from_secs(30),
        }
    }

    /// Run forever, syncing every volume this node is primary for once its
    /// interval elapses. Intended to be spawned as a background task.
    pub async fn run(&self, intervals: &dyn IntervalSource) {
        let mut next_due: std::collections::BTreeMap<VolumeId, Instant> =
            std::collections::BTreeMap::new();
        let mut ticker = tokio::time::interval(self.tick);
        // Volumes last seen frozen: when one unfreezes it is pushed at once,
        // not at the end of its (possibly long) interval.
        let mut was_frozen: BTreeSet<VolumeId> = BTreeSet::new();

        loop {
            ticker.tick().await;

            if !self.quorum.has_quorum() {
                debug!("no quorum, skipping this sync pass");
                continue;
            }

            let state = self.node.state().await;
            let now = Instant::now();

            self.remove_deleted_volumes(&state.volume_removals).await;

            for (id, volume) in &state.volumes {
                if !self.registry.implements(&id.drive) {
                    continue;
                }
                // Frozen for hand edits: nothing may sync out of, or over,
                // this volume until it is unfrozen.
                if volume.frozen.is_some() {
                    was_frozen.insert(id.clone());
                    continue;
                }
                if was_frozen.remove(id) {
                    next_due.insert(id.clone(), now);
                }
                let is_served = self.registry.is_served(&id.drive);
                let is_source = if is_served {
                    self.registry.active_server(&id.drive, &state.unreachable)
                        == Some(self.registry.node())
                } else {
                    volume.primary == self.registry.node()
                };
                if !is_source {
                    continue;
                }

                let interval = intervals.interval_for(id);
                let due_at = next_due.entry(id.clone()).or_insert_with(|| {
                    let spread: f64 = rand::thread_rng().gen();
                    now + jitter(interval, spread)
                });
                if now < *due_at {
                    continue;
                }

                if !self.guard.try_start(id).await {
                    // Previous run is still going; wait for the next tick
                    // rather than piling up a second one.
                    continue;
                }
                *due_at = now + interval;

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
                        .filter(|(name, n)| {
                            *name != self.registry.node() && n.implements(&id.drive)
                        })
                        .map(|(name, n)| ReplicaTarget {
                            node: name.clone(),
                            addr: n.rpc_addr,
                        })
                        .collect()
                };

                self.check_empty(id, volume.looks_empty).await;
                self.run_one(id.clone(), volume.generation, targets).await;
            }
        }
    }

    /// Removes this node's copy of every volume deleted on purpose, then says
    /// so. A copy that has gained a file since is left alone and not
    /// acknowledged: only a file-less tree is ever deleted here.
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
                    match crate::scan::scan_off_runtime(path.clone(), false).await {
                        Ok(m) if m.files.is_empty() => {
                            if let Err(e) = std::fs::remove_dir_all(&path) {
                                warn!(%id, error = %e, "could not delete the volume directory");
                                continue;
                            }
                        }
                        Ok(_) => {
                            warn!(%id, "kept a deleted volume's directory: it holds files");
                            continue;
                        }
                        Err(e) => {
                            warn!(%id, error = %e, "could not check the volume before deleting it");
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

    /// Raises `looks_empty` once this volume has had no files on two passes
    /// running, and lowers it as soon as one shows up. Nothing is done about
    /// an empty volume here: whether it should be deleted or restored is the
    /// operator's call.
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
            let mut seen = self.empty_seen.lock().unwrap_or_else(|e| e.into_inner());
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

    /// Surfaces a refused push as a cluster problem. Skipped when the same
    /// refusal is already recorded, so a volume refused every pass does not
    /// write to the raft log every pass.
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

    async fn run_one(&self, id: VolumeId, generation: u64, targets: Vec<ReplicaTarget>) {
        let src = match self.registry.volume_path(&id) {
            Ok(path) => path,
            Err(e) => {
                warn!(%id, error = %e, "cannot resolve local volume path");
                self.guard.finish(&id).await;
                return;
            }
        };

        for target in targets {
            // A freeze can land while earlier targets of this run were
            // syncing; stop pushing the moment it does.
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

        self.guard.finish(&id).await;
    }
}

/// A static map, for callers that have already resolved every volume's
/// interval (the common case: the daemon does this once per stack deploy).
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
    fn interval_for(&self, volume: &VolumeId) -> Duration {
        resolve_interval(self.default, self.overrides.get(volume).copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let volume = VolumeId::new("fast", "data");
        let intervals = StaticIntervals::new(
            Duration::from_secs(900),
            std::collections::BTreeMap::from([(volume.clone(), Duration::from_secs(30))]),
        );
        assert_eq!(intervals.interval_for(&volume), Duration::from_secs(30));
        assert_eq!(
            intervals.interval_for(&VolumeId::new("fast", "other")),
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
