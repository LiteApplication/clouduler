//! Node failure handling. Two halves tuned against each other:
//!
//! - The leader probes every node and, once one is silent longer than
//!   `node_timeout`, marks it unreachable so the scheduler moves its work.
//! - Every node watches its own quorum and, once without one longer than
//!   `fence_timeout`, stops its containers.
//!
//! Fencing must happen before rescheduling, or a container runs in two places
//! with two writers on a shared volume. The config validator enforces
//! `fence_timeout < node_timeout`; the gap is the safety margin.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Tracks when each node last answered a probe; time is injected for testing.
#[derive(Debug, Default)]
pub struct LivenessTracker {
    last_seen: BTreeMap<String, Instant>,
}

impl LivenessTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seen(&mut self, node: &str, at: Instant) {
        self.last_seen.insert(node.to_string(), at);
    }

    /// Start the clock for a never-probed node, so it cannot look fine forever.
    pub fn track(&mut self, node: &str, at: Instant) {
        self.last_seen.entry(node.to_string()).or_insert(at);
    }

    pub fn forget(&mut self, node: &str) {
        self.last_seen.remove(node);
    }

    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.last_seen.retain(|node, _| keep(node));
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceState {
    Open,
    /// Daemon restarted shortly after last having quorum: the leader cannot have
    /// moved this node's work yet. Running containers keep running but nothing is
    /// created, started, stopped or removed until quorum is confirmed or
    /// `fence_timeout` runs out (then `Closed`).
    Hold,
    /// No quorum, or none confirmed yet: containers are stopped, none started.
    Closed,
}

impl FenceState {
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::Hold => 1,
            Self::Closed => 2,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Open,
            1 => Self::Hold,
            _ => Self::Closed,
        }
    }
}

#[derive(Debug)]
pub struct Fence {
    lost_at: Option<Instant>,
    state: FenceState,
    /// Whether `observe` has run, to tell "fenced" from "closed by default"
    /// (see `is_observed`).
    observed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceAction {
    None,
    /// Quorum gone too long: stop the containers running here.
    Fence,
    /// Quorum is back: the reconcile loop may act again.
    Release,
}

impl Default for Fence {
    fn default() -> Self {
        Self::new()
    }
}

impl Fence {
    /// A new fence is closed. A rebooted node's stored state still claims the
    /// containers it owned, and reconcile would act on that before raft says the
    /// leader handed them elsewhere; starting open would run a second copy of
    /// everything. Starting closed costs one probe interval.
    pub fn new() -> Self {
        Self {
            lost_at: None,
            state: FenceState::Closed,
            observed: false,
        }
    }

    /// A fence for a daemon restarting `since_quorum` after last having quorum
    /// (already checked to be below `node_timeout`): stopping everything would only
    /// cause an outage. The loss clock starts at that last quorum, not the restart,
    /// since the leader's `node_timeout` runs from the same moment.
    pub fn resume(now: Instant, since_quorum: Duration) -> Self {
        Self {
            lost_at: Some(now.checked_sub(since_quorum).unwrap_or(now)),
            state: FenceState::Hold,
            observed: false,
        }
    }

    /// Feed the current quorum state and get back what to do. A flapping quorum
    /// does not stop containers; only a sustained loss fences.
    pub fn observe(&mut self, has_quorum: bool, now: Instant, timeout: Duration) -> FenceAction {
        self.observed = true;
        if has_quorum {
            self.lost_at = None;
            if self.state != FenceState::Open {
                self.state = FenceState::Open;
                return FenceAction::Release;
            }
            return FenceAction::None;
        }

        let lost_at = *self.lost_at.get_or_insert(now);
        if self.state != FenceState::Closed && now.duration_since(lost_at) >= timeout {
            self.state = FenceState::Closed;
            return FenceAction::Fence;
        }
        FenceAction::None
    }

    pub fn state(&self) -> FenceState {
        self.state
    }

    pub fn is_fenced(&self) -> bool {
        self.state == FenceState::Closed
    }

    /// Whether `observe` has been called. Before that, a fence is closed only as a
    /// default, not because quorum was checked and found missing.
    pub fn is_observed(&self) -> bool {
        self.observed
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

        assert!(tracker
            .expired(at(base, 20), Duration::from_secs(30))
            .is_empty());
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

        assert!(tracker
            .expired(at(base, 40), Duration::from_secs(30))
            .is_empty());
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

    /// A fence on a node that has been in the cluster for a while.
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

        // Just booted: must not run anything, however long that lasts.
        assert!(fence.is_fenced());
        assert_eq!(fence.observe(false, base, timeout), FenceAction::None);
        assert_eq!(
            fence.observe(false, at(base, 60), timeout),
            FenceAction::None
        );
        assert!(fence.is_fenced());

        // and it opens on the first sighting of a quorum, not a timeout later.
        assert_eq!(
            fence.observe(true, at(base, 61), timeout),
            FenceAction::Release
        );
        assert!(!fence.is_fenced());
    }

    #[test]
    fn is_observed_only_after_the_first_call() {
        let base = Instant::now();
        let timeout = Duration::from_secs(15);

        let mut fence = Fence::new();
        assert!(!fence.is_observed());

        // Must flip on the call itself, not on a particular result.
        fence.observe(false, base, timeout);
        assert!(fence.is_observed());
    }

    #[test]
    fn a_brief_quorum_loss_does_not_fence() {
        let base = Instant::now();
        let timeout = Duration::from_secs(15);
        let mut fence = settled(base, timeout);

        assert_eq!(fence.observe(false, base, timeout), FenceAction::None);
        assert_eq!(
            fence.observe(false, at(base, 5), timeout),
            FenceAction::None
        );
        assert_eq!(fence.observe(true, at(base, 6), timeout), FenceAction::None);
        assert!(!fence.is_fenced());
    }

    #[test]
    fn a_sustained_quorum_loss_fences_once() {
        let base = Instant::now();
        let timeout = Duration::from_secs(15);
        let mut fence = settled(base, timeout);

        assert_eq!(fence.observe(false, base, timeout), FenceAction::None);
        assert_eq!(
            fence.observe(false, at(base, 15), timeout),
            FenceAction::Fence
        );
        // Staying fenced must not keep firing the stop action.
        assert_eq!(
            fence.observe(false, at(base, 30), timeout),
            FenceAction::None
        );
        assert!(fence.is_fenced());
    }

    #[test]
    fn regaining_quorum_releases_the_fence() {
        let base = Instant::now();
        let timeout = Duration::from_secs(15);
        let mut fence = settled(base, timeout);

        fence.observe(false, base, timeout);
        fence.observe(false, at(base, 15), timeout);
        assert_eq!(
            fence.observe(true, at(base, 20), timeout),
            FenceAction::Release
        );
        assert!(!fence.is_fenced());
        // Release fires once, not on every later tick.
        assert_eq!(
            fence.observe(true, at(base, 21), timeout),
            FenceAction::None
        );
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
        assert_eq!(
            fence.observe(false, at(base, 20), timeout),
            FenceAction::None
        );
        assert_eq!(
            fence.observe(false, at(base, 27), timeout),
            FenceAction::Fence
        );
    }

    #[test]
    fn a_resumed_fence_holds_and_releases_on_quorum() {
        let base = Instant::now();
        let timeout = Duration::from_secs(30);
        let mut fence = Fence::resume(base, Duration::from_secs(5));

        // Not fenced, so nothing running is stopped, but not open either.
        assert_eq!(fence.state(), FenceState::Hold);
        assert!(!fence.is_fenced());
        assert_eq!(fence.observe(false, base, timeout), FenceAction::None);
        assert_eq!(fence.state(), FenceState::Hold);

        assert_eq!(
            fence.observe(true, at(base, 3), timeout),
            FenceAction::Release
        );
        assert_eq!(fence.state(), FenceState::Open);
    }

    #[test]
    fn a_resumed_fence_closes_when_quorum_never_comes() {
        let base = Instant::now();
        let timeout = Duration::from_secs(30);
        let mut fence = Fence::resume(base, Duration::ZERO);

        assert_eq!(
            fence.observe(false, at(base, 29), timeout),
            FenceAction::None
        );
        assert_eq!(fence.state(), FenceState::Hold);
        assert_eq!(
            fence.observe(false, at(base, 30), timeout),
            FenceAction::Fence
        );
        assert!(fence.is_fenced());
        // and then stays closed like any other fence.
        assert_eq!(
            fence.observe(false, at(base, 40), timeout),
            FenceAction::None
        );
    }

    #[test]
    fn a_resumed_fence_counts_from_the_last_quorum_not_the_restart() {
        let base = Instant::now();
        let timeout = Duration::from_secs(30);
        // Quorum was last seen 20s before this restart, so only 10s remain.
        let mut fence = Fence::resume(base, Duration::from_secs(20));

        assert_eq!(
            fence.observe(false, at(base, 9), timeout),
            FenceAction::None
        );
        assert_eq!(
            fence.observe(false, at(base, 10), timeout),
            FenceAction::Fence
        );
    }

    #[test]
    fn a_fresh_fence_never_holds() {
        // The closed start a stale or missing quorum record gets is unchanged.
        let base = Instant::now();
        let mut fence = Fence::new();
        assert_eq!(fence.state(), FenceState::Closed);
        fence.observe(false, base, Duration::from_secs(30));
        assert_eq!(fence.state(), FenceState::Closed);
    }

    #[test]
    fn the_state_survives_a_round_trip_through_a_byte() {
        for s in [FenceState::Open, FenceState::Hold, FenceState::Closed] {
            assert_eq!(FenceState::from_u8(s.as_u8()), s);
        }
        assert_eq!(FenceState::from_u8(200), FenceState::Closed);
    }
}
