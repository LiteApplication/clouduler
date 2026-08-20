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
            tick: Duration::from_secs(30),
        }
    }

    /// Run forever, syncing every volume this node is primary for once its
    /// interval elapses. Intended to be spawned as a background task.
    pub async fn run(&self, intervals: &dyn IntervalSource) {
        let mut next_due: std::collections::BTreeMap<VolumeId, Instant> = std::collections::BTreeMap::new();
        let mut ticker = tokio::time::interval(self.tick);

        loop {
            ticker.tick().await;

            if !self.quorum.has_quorum() {
                debug!("no quorum, skipping this sync pass");
                continue;
            }

            let state = self.node.state().await;
            let now = Instant::now();

            for (id, volume) in &state.volumes {
                if !self.registry.implements(&id.drive) {
                    continue;
                }
                let is_served = self.registry.is_served(&id.drive);
                let is_source = if is_served {
                    self.registry.active_server(&id.drive, &state.unreachable) == Some(self.registry.node())
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
                            state.nodes.get(name).map(|n| ReplicaTarget { node: name.clone(), addr: n.rpc_addr })
                        })
                        .collect()
                } else {
                    state
                        .nodes
                        .iter()
                        .filter(|(name, n)| *name != self.registry.node() && n.implements(&id.drive))
                        .map(|(name, n)| ReplicaTarget { node: name.clone(), addr: n.rpc_addr })
                        .collect()
                };

                self.run_one(id.clone(), volume.generation, targets).await;
            }
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
            let drive_addr = crate::transport::drive_port(target.addr.port());
            let peer = std::net::SocketAddr::new(target.addr.ip(), drive_addr);

            match self.engine.sync(&src, peer, &id, generation).await {
                Ok(stats) => {
                    debug!(%id, node = %target.node, ?stats, "volume synced");
                    if let Err(e) = self
                        .node
                        .submit(Request::SetVolumeReplica { id: id.clone(), node: target.node.clone(), generation })
                        .await
                    {
                        warn!(%id, node = %target.node, error = %e, "failed to record replica progress");
                    }
                }
                Err(e) => warn!(%id, node = %target.node, error = %e, "volume sync failed"),
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
    pub fn new(default: Duration, overrides: std::collections::BTreeMap<VolumeId, Duration>) -> Self {
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
        assert!(jitter(interval, 1.5) < interval, "out of range spread is clamped");
    }

    #[test]
    fn static_intervals_prefer_the_override() {
        let volume = VolumeId::new("fast", "data");
        let intervals = StaticIntervals::new(
            Duration::from_secs(900),
            std::collections::BTreeMap::from([(volume.clone(), Duration::from_secs(30))]),
        );
        assert_eq!(intervals.interval_for(&volume), Duration::from_secs(30));
        assert_eq!(intervals.interval_for(&VolumeId::new("fast", "other")), Duration::from_secs(900));
    }

    #[tokio::test]
    async fn in_flight_guard_refuses_a_second_start() {
        let guard = InFlightGuard::new();
        let id = VolumeId::new("fast", "data");
        assert!(guard.try_start(&id).await);
        assert!(!guard.try_start(&id).await, "second start should be refused");
        assert!(guard.is_running(&id).await);
        guard.finish(&id).await;
        assert!(!guard.is_running(&id).await);
        assert!(guard.try_start(&id).await, "start allowed again after finish");
    }

    #[tokio::test]
    async fn in_flight_guard_tracks_volumes_independently() {
        let guard = InFlightGuard::new();
        let a = VolumeId::new("fast", "a");
        let b = VolumeId::new("fast", "b");
        assert!(guard.try_start(&a).await);
        assert!(guard.try_start(&b).await, "a different volume is unaffected");
    }
}
