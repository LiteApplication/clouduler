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

/// What this node may do with containers right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceState {
    /// Quorum is held: reconcile normally.
    Open,
    /// The daemon restarted shortly after last having a quorum, so the leader
    /// cannot have moved this node's work yet. Whatever runs keeps running,
    /// but nothing is created, started, stopped or removed until quorum is
    /// confirmed, or `fence_timeout` runs out and this becomes `Closed`.
    Hold,
    /// No quorum, or none confirmed yet: running containers are stopped and
    /// nothing is started.
    Closed,
}

impl FenceState {
    /// For sharing the state through an `AtomicU8`.
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::Hold => 1,
            Self::Closed => 2,
        }
    }

    /// Anything unrecognised reads as `Closed`, the safe answer.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Open,
            1 => Self::Hold,
            _ => Self::Closed,
        }
    }
}

/// Tracks how long this node has been without a quorum.
#[derive(Debug)]
pub struct Fence {
    /// When quorum was lost, or None while it is held.
    lost_at: Option<Instant>,
    state: FenceState,
    /// Whether `observe` has run at least once. A caller needs this to tell
    /// "genuinely fenced" apart from "still closed because nobody has asked
    /// yet" — see `is_observed`.
    observed: bool,
}

/// What the fence wants the node to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceAction {
    /// Nothing to do.
    None,
    /// Quorum has been gone too long: stop the containers running here.
    Fence,
    /// Quorum is back (from `Closed` or `Hold`): the reconcile loop may act
    /// again.
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
            state: FenceState::Closed,
            observed: false,
        }
    }

    /// A fence for a daemon restarting `since_quorum` after this node last
    /// had a quorum, which the caller has already checked is shorter than
    /// `node_timeout`: the leader cannot have rescheduled this node's work
    /// yet, so stopping it all would only cause an outage.
    ///
    /// The loss clock starts at that last quorum rather than at the restart.
    /// The leader's `node_timeout` runs from the same moment, so a fence
    /// counted from the restart could land after the leader has already moved
    /// the work.
    pub fn resume(now: Instant, since_quorum: Duration) -> Self {
        Self {
            lost_at: Some(now.checked_sub(since_quorum).unwrap_or(now)),
            state: FenceState::Hold,
            observed: false,
        }
    }

    /// Feed the current quorum state and get back what to do.
    ///
    /// Containers keep running while quorum is merely flapping, because killing
    /// them on a brief blip turns a network hiccup into an outage. Only a
    /// sustained loss fences.
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

    /// Whether `observe` has been called yet. A fresh fence is closed
    /// (`is_fenced() == true`) before its first observation purely as a
    /// starting default, not because quorum was actually checked and found
    /// missing — a caller that needs to distinguish those two cases (e.g. to
    /// avoid acting on a fence result that was never really decided) should
    /// check this first.
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

        // True regardless of what that first call finds: a caller telling
        // "fenced because unchecked" apart from "fenced because confirmed"
        // needs this to flip on the call itself, not on a particular result.
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
