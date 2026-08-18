//! Node failure handling.
//!
//! Two halves that have to be tuned against each other:
//!
//! - The leader probes every node and, once one has been silent for longer than
//!   `node_timeout`, marks it unreachable so the scheduler moves its work.
//! - Every node watches its own quorum and, once it has been without one for
//!   longer than `fence_timeout`, stops the containers it is running.
//!
//! Fencing has to happen before rescheduling, or the same container runs in two
//! places and a shared volume gets two writers. The config validator enforces
//! `fence_timeout < node_timeout`, and both are timed from the same kind of
//! event, so the gap between them is the safety margin.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Tracks when each node last answered a probe. Timekeeping is injected so the
/// decision logic can be tested without sleeping.
#[derive(Debug, Default)]
pub struct LivenessTracker {
    last_seen: BTreeMap<String, Instant>,
}

impl LivenessTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful probe.
    pub fn seen(&mut self, node: &str, at: Instant) {
        self.last_seen.insert(node.to_string(), at);
    }

    /// Start the clock for a node we have not probed yet. Without this, a node
    /// that was never reachable would look fine forever.
    pub fn track(&mut self, node: &str, at: Instant) {
        self.last_seen.entry(node.to_string()).or_insert(at);
    }

    pub fn forget(&mut self, node: &str) {
        self.last_seen.remove(node);
    }

    /// Drop nodes that are no longer in the roster.
    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.last_seen.retain(|node, _| keep(node));
    }

    /// Nodes silent for longer than `timeout` as of `now`.
    pub fn expired(&self, now: Instant, timeout: Duration) -> Vec<String> {
        self.last_seen
            .iter()
            .filter(|(_, seen)| now.duration_since(**seen) > timeout)
            .map(|(node, _)| node.clone())
            .collect()
    }

    pub fn is_live(&self, node: &str, now: Instant, timeout: Duration) -> bool {
        self.last_seen
            .get(node)
            .is_some_and(|seen| now.duration_since(*seen) <= timeout)
    }
}

/// Tracks how long this node has been without a quorum.
#[derive(Debug)]
pub struct Fence {
    /// When quorum was lost, or None while it is held.
    lost_at: Option<Instant>,
    fenced: bool,
}

/// What the fence wants the node to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceAction {
    /// Nothing to do.
    None,
    /// Quorum has been gone too long: stop the containers running here.
    Fence,
    /// Quorum is back: the reconcile loop may start containers again.
    Release,
}

impl Default for Fence {
    fn default() -> Self {
        Self::new()
    }
}

impl Fence {
    /// A new fence is closed.
    ///
    /// This is what a node coming back from a reboot has to start from. Its
    /// stored state still says it owns the containers it owned when it died,
    /// and the reconcile loop would act on that within a second of boot, well
    /// before raft has told it that the leader handed those containers to
    /// somebody else. Starting open means a rejoining node starts a second
    /// copy of everything it used to run; starting closed costs one probe
    /// interval on a node that really is still in the cluster.
    pub fn new() -> Self {
        Self {
            lost_at: None,
            fenced: true,
        }
    }

    /// Feed the current quorum state and get back what to do.
    ///
    /// Containers keep running while quorum is merely flapping, because killing
    /// them on a brief blip turns a network hiccup into an outage. Only a
    /// sustained loss fences.
    pub fn observe(&mut self, has_quorum: bool, now: Instant, timeout: Duration) -> FenceAction {
        if has_quorum {
            self.lost_at = None;
            if self.fenced {
                self.fenced = false;
                return FenceAction::Release;
            }
            return FenceAction::None;
        }

        let lost_at = *self.lost_at.get_or_insert(now);
        if !self.fenced && now.duration_since(lost_at) >= timeout {
            self.fenced = true;
            return FenceAction::Fence;
        }
        FenceAction::None
    }

    pub fn is_fenced(&self) -> bool {
        self.fenced
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn a_node_expires_only_after_the_timeout() {
        let base = Instant::now();
        let mut tracker = LivenessTracker::new();
        tracker.seen("n1", base);

        assert!(tracker.expired(at(base, 20), Duration::from_secs(30)).is_empty());
        assert_eq!(
            tracker.expired(at(base, 31), Duration::from_secs(30)),
            vec!["n1".to_string()]
        );
    }

    #[test]
    fn answering_again_clears_the_expiry() {
        let base = Instant::now();
        let mut tracker = LivenessTracker::new();
        tracker.seen("n1", base);
        tracker.seen("n1", at(base, 31));

        assert!(tracker.expired(at(base, 40), Duration::from_secs(30)).is_empty());
        assert!(tracker.is_live("n1", at(base, 40), Duration::from_secs(30)));
    }

    #[test]
    fn a_node_never_reached_still_expires() {
        let base = Instant::now();
        let mut tracker = LivenessTracker::new();
        tracker.track("n1", base);
        assert_eq!(
            tracker.expired(at(base, 31), Duration::from_secs(30)),
            vec!["n1".to_string()]
        );
    }

    #[test]
    fn tracking_does_not_overwrite_a_real_sighting() {
        let base = Instant::now();
        let mut tracker = LivenessTracker::new();
        tracker.seen("n1", base);
        tracker.track("n1", at(base, 60));
        assert!(!tracker.is_live("n1", at(base, 61), Duration::from_secs(30)));
    }

    /// A fence on a node that has been up and in the cluster for a while.
    /// Every test but the boot one starts here, since a fresh `Fence` is
    /// closed and the interesting transitions are the ones after that.
    fn settled(base: Instant, timeout: Duration) -> Fence {
        let mut fence = Fence::new();
        assert_eq!(fence.observe(true, base, timeout), FenceAction::Release);
        fence
    }

    #[test]
    fn a_fresh_fence_is_closed_until_quorum_is_confirmed() {
        let base = Instant::now();
        let mut fence = Fence::new();
        let timeout = Duration::from_secs(15);

        // A node that has just booted has not heard from anyone yet. It must
        // not run anything in that window, however long it lasts.
        assert!(fence.is_fenced());
        assert_eq!(fence.observe(false, base, timeout), FenceAction::None);
        assert_eq!(fence.observe(false, at(base, 60), timeout), FenceAction::None);
        assert!(fence.is_fenced());

        // and it opens on the first sighting of a quorum, not a timeout later.
        assert_eq!(fence.observe(true, at(base, 61), timeout), FenceAction::Release);
        assert!(!fence.is_fenced());
    }

    #[test]
    fn a_brief_quorum_loss_does_not_fence() {
        let base = Instant::now();
        let timeout = Duration::from_secs(15);
        let mut fence = settled(base, timeout);

        assert_eq!(fence.observe(false, base, timeout), FenceAction::None);
        assert_eq!(fence.observe(false, at(base, 5), timeout), FenceAction::None);
        assert_eq!(fence.observe(true, at(base, 6), timeout), FenceAction::None);
        assert!(!fence.is_fenced());
    }

    #[test]
    fn a_sustained_quorum_loss_fences_once() {
        let base = Instant::now();
        let timeout = Duration::from_secs(15);
        let mut fence = settled(base, timeout);

        assert_eq!(fence.observe(false, base, timeout), FenceAction::None);
        assert_eq!(fence.observe(false, at(base, 15), timeout), FenceAction::Fence);
        // Staying fenced must not keep firing the stop action.
        assert_eq!(fence.observe(false, at(base, 30), timeout), FenceAction::None);
        assert!(fence.is_fenced());
    }

    #[test]
    fn regaining_quorum_releases_the_fence() {
        let base = Instant::now();
        let timeout = Duration::from_secs(15);
        let mut fence = settled(base, timeout);

        fence.observe(false, base, timeout);
        fence.observe(false, at(base, 15), timeout);
        assert_eq!(fence.observe(true, at(base, 20), timeout), FenceAction::Release);
        assert!(!fence.is_fenced());
        // Release fires once, not on every later tick.
        assert_eq!(fence.observe(true, at(base, 21), timeout), FenceAction::None);
    }

    #[test]
    fn the_loss_clock_restarts_after_recovery() {
        let base = Instant::now();
        let timeout = Duration::from_secs(15);
        let mut fence = settled(base, timeout);

        fence.observe(false, base, timeout);
        fence.observe(true, at(base, 10), timeout);
        // A second dip starts counting from scratch, not from the first one.
        fence.observe(false, at(base, 12), timeout);
        assert_eq!(fence.observe(false, at(base, 20), timeout), FenceAction::None);
        assert_eq!(fence.observe(false, at(base, 27), timeout), FenceAction::Fence);
    }
}
