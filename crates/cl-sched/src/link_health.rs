//! A short rolling history of this node's probes of each peer, shared by the api (which probes and
//! reports it) and the scheduler (which avoids moving instances while links are unstable).
//!
//! Node-local and in memory: each node records what *it* sees, and losing the history on restart
//! only means the state reads `ok` until the next probes say otherwise.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Samples kept per peer. At the api's probe interval this is a few minutes.
pub const HISTORY_LEN: usize = 30;

/// Trailing samples that must all have failed for a link to read `down` rather than `flapping`.
pub const DOWN_AFTER: usize = 3;

/// Failed samples in the window from which a link that is not down reads `flapping`.
pub const FLAPPING_FAILURES: usize = 2;

/// One probe round of one link, as seen from this node.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkSample {
    /// Unix seconds.
    pub at: i64,
    pub raft: bool,
    pub api: bool,
    /// The drive-sync port.
    pub drive: bool,
    pub raft_ms: Option<u64>,
}

impl LinkSample {
    /// Every probed port answered.
    pub fn up(&self) -> bool {
        self.raft && self.api && self.drive
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LinkState {
    /// No more than one failed sample in the window, or nothing recorded yet.
    #[default]
    Ok,
    /// Failures keep coming and going: at least `FLAPPING_FAILURES` in the window, but the newest
    /// samples are not all failures.
    Flapping,
    /// The newest `DOWN_AFTER` samples (fewer if that is all there is) all failed.
    Down,
}

/// What the history says about one link.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LinkSummary {
    pub state: LinkState,
    /// Failed samples in `history`.
    pub failures: usize,
    /// Oldest first.
    pub history: Vec<LinkSample>,
}

/// Pure so it is testable without a clock or a network.
pub fn classify(history: &[LinkSample]) -> LinkState {
    let Some(_) = history.last() else {
        return LinkState::Ok;
    };
    let failures = history.iter().filter(|s| !s.up()).count();
    let tail = history.len().min(DOWN_AFTER);
    if history[history.len() - tail..].iter().all(|s| !s.up()) {
        LinkState::Down
    } else if failures >= FLAPPING_FAILURES {
        LinkState::Flapping
    } else {
        LinkState::Ok
    }
}

/// Cheap to clone; every clone shares the same history.
#[derive(Clone, Default)]
pub struct LinkHealth {
    peers: Arc<Mutex<BTreeMap<String, VecDeque<LinkSample>>>>,
}

impl LinkHealth {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, peer: &str, sample: LinkSample) {
        let mut peers = self.peers.lock();
        let history = peers.entry(peer.to_string()).or_default();
        history.push_back(sample);
        while history.len() > HISTORY_LEN {
            history.pop_front();
        }
    }

    /// Drops peers that left the config, so a removed node does not stay flapping forever.
    pub fn retain_peers<'a>(&self, keep: impl IntoIterator<Item = &'a str>) {
        let keep: Vec<&str> = keep.into_iter().collect();
        self.peers
            .lock()
            .retain(|name, _| keep.contains(&name.as_str()));
    }

    pub fn summary(&self, peer: &str) -> LinkSummary {
        let peers = self.peers.lock();
        let history: Vec<LinkSample> = peers
            .get(peer)
            .map(|h| h.iter().copied().collect())
            .unwrap_or_default();
        LinkSummary {
            state: classify(&history),
            failures: history.iter().filter(|s| !s.up()).count(),
            history,
        }
    }

    pub fn state(&self, peer: &str) -> LinkState {
        let peers = self.peers.lock();
        peers
            .get(peer)
            .map(|h| classify(&h.iter().copied().collect::<Vec<_>>()))
            .unwrap_or_default()
    }

    /// Whether this node's link to `peer` is flapping or down.
    pub fn is_unstable(&self, peer: &str) -> bool {
        self.state(peer) != LinkState::Ok
    }

    /// Whether any of this node's links is flapping (down links are not counted: a peer that is
    /// simply gone says nothing about this node's own network).
    pub fn any_flapping(&self) -> bool {
        let peers = self.peers.lock();
        peers
            .values()
            .any(|h| classify(&h.iter().copied().collect::<Vec<_>>()) == LinkState::Flapping)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(up: bool) -> LinkSample {
        LinkSample {
            at: 0,
            raft: up,
            api: up,
            drive: up,
            raft_ms: up.then_some(1),
        }
    }

    fn run(pattern: &str) -> Vec<LinkSample> {
        pattern.chars().map(|c| s(c == '+')).collect()
    }

    #[test]
    fn classifies_by_failures_and_trailing_run() {
        assert_eq!(classify(&[]), LinkState::Ok);
        assert_eq!(classify(&run("++++++")), LinkState::Ok);
        assert_eq!(
            classify(&run("+++-++")),
            LinkState::Ok,
            "one blip is not flapping"
        );
        assert_eq!(classify(&run("++-+-+")), LinkState::Flapping);
        assert_eq!(
            classify(&run("+--+++")),
            LinkState::Flapping,
            "recovered outage"
        );
        assert_eq!(classify(&run("++---")), LinkState::Down);
        assert_eq!(classify(&run("-")), LinkState::Down);
        assert_eq!(classify(&run("+-")), LinkState::Ok);
    }

    #[test]
    fn one_failing_port_fails_the_sample() {
        let mut sample = s(true);
        sample.drive = false;
        assert!(!sample.up());
        assert_eq!(classify(&[sample, sample, sample]), LinkState::Down);
    }

    #[test]
    fn history_is_bounded_and_shared_between_clones() {
        let health = LinkHealth::new();
        let other = health.clone();
        for _ in 0..HISTORY_LEN + 10 {
            other.record("b", s(true));
        }
        assert_eq!(health.summary("b").history.len(), HISTORY_LEN);
        assert_eq!(health.summary("nobody").history.len(), 0);
        assert!(!health.is_unstable("b"));
    }

    #[test]
    fn unstable_and_flapping_helpers() {
        let health = LinkHealth::new();
        for c in "++-+-+".chars() {
            health.record("flappy", s(c == '+'));
        }
        for _ in 0..4 {
            health.record("gone", s(false));
        }
        assert!(health.is_unstable("flappy"));
        assert!(health.is_unstable("gone"));
        assert!(health.any_flapping());
        health.retain_peers(["gone"]);
        assert!(!health.any_flapping(), "down alone is not flapping");
        assert_eq!(health.summary("flappy"), LinkSummary::default());
    }
}
