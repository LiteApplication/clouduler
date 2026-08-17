//! When we last heard from the leader.
//!
//! Raft's own metrics only tell a leader whether it still holds quorum. A
//! follower, and a learner especially, keeps reporting the last leader it knew
//! about long after that leader has gone, because it has nothing to replace it
//! with. Timing the arrival of leader traffic ourselves is what lets a
//! partitioned node work out that it is on its own.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Sentinel for "no contact yet", so the common path stays a plain atomic load.
const NEVER: u64 = u64::MAX;

#[derive(Clone)]
pub struct Contact {
    base: Instant,
    millis: Arc<AtomicU64>,
}

impl Contact {
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            millis: Arc::new(AtomicU64::new(NEVER)),
        }
    }

    /// Record leader traffic. Called on the RPC path, so it stays a single
    /// atomic store.
    pub fn touch(&self) {
        let elapsed = self.base.elapsed().as_millis() as u64;
        self.millis.store(elapsed, Ordering::Relaxed);
    }

    /// How long since the last contact, or None if there has never been any.
    pub fn since(&self) -> Option<Duration> {
        match self.millis.load(Ordering::Relaxed) {
            NEVER => None,
            at => Some(
                self.base
                    .elapsed()
                    .saturating_sub(Duration::from_millis(at)),
            ),
        }
    }

    /// Whether the leader has been in touch within `timeout`.
    pub fn is_fresh(&self, timeout: Duration) -> bool {
        self.since().is_some_and(|since| since <= timeout)
    }
}

impl Default for Contact {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_contact_is_never_fresh() {
        let contact = Contact::new();
        assert!(contact.since().is_none());
        assert!(!contact.is_fresh(Duration::from_secs(60)));
    }

    #[test]
    fn a_touch_is_immediately_fresh() {
        let contact = Contact::new();
        contact.touch();
        assert!(contact.is_fresh(Duration::from_secs(1)));
        assert!(contact.since().expect("some") < Duration::from_millis(100));
    }

    #[test]
    fn contact_goes_stale() {
        let contact = Contact::new();
        contact.touch();
        std::thread::sleep(Duration::from_millis(30));
        assert!(!contact.is_fresh(Duration::from_millis(5)));
    }

    #[test]
    fn clones_share_one_clock() {
        let contact = Contact::new();
        let other = contact.clone();
        other.touch();
        assert!(contact.is_fresh(Duration::from_secs(1)));
    }
}
