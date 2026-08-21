//! The progress object for one self-update run.
//!
//! This is exactly what gets stored, as json, in the reserved config object
//! `_clouduler_self_update_run`: it rides the existing config CAS plumbing and
//! the existing `/api/events` SSE stream for free, so no new raft entry type
//! is needed for it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::release::ReleaseAsset;

/// Reserved config name the run state is stored under. Never exposed through
/// the generic `/api/configs` routes.
pub const RUN_CONFIG: &str = "_clouduler_self_update_run";

/// Reserved secret name the sealed GitHub API token is stored under. Never
/// exposed through the generic `/api/secrets` routes. Optional: only needed
/// for a private repo, or to avoid GitHub's low unauthenticated rate limit.
pub const GITHUB_TOKEN_SECRET: &str = "_clouduler_self_update_github_token";

/// One node's progress through a run, in the order it must be crossed.
///
/// Forward-only, like `cl_raft::state::MigrationPhase`: a stale message must
/// never rewind a node's phase, since two workers racing to report the same
/// step would otherwise be able to bounce it back and forth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodePhase {
    Idle,
    Fetching,
    FetchFailed { reason: String },
    Ready,
    Restarting,
    Done,
    RestartFailed { reason: String },
}

impl NodePhase {
    /// The phase this one may advance to on success. `None` for a phase that
    /// is either terminal or whose next step the caller decides (`Fetching`
    /// ends at either `Ready` or `FetchFailed`, decided by the worker).
    pub fn next(&self) -> Option<NodePhase> {
        use NodePhase::*;
        match self {
            Idle => Some(Fetching),
            Ready => Some(Restarting),
            Restarting => Some(Done),
            Fetching | FetchFailed { .. } | Done | RestartFailed { .. } => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, NodePhase::Done | NodePhase::FetchFailed { .. } | NodePhase::RestartFailed { .. })
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, NodePhase::FetchFailed { .. } | NodePhase::RestartFailed { .. })
    }
}

/// Everything the orchestrator and every worker agree on for one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateRunState {
    /// Unique per run, so a worker that missed the start of a run (restarted
    /// mid-flight) can tell "still the run I was doing" from "a new one".
    pub id: String,
    /// Set once the leader has resolved the release this run targets, along
    /// with `owner`/`repo`, needed by workers to build each asset's download
    /// url.
    pub release_tag: Option<String>,
    pub owner: Option<String>,
    pub repo: Option<String>,
    /// arch -> the asset resolved for it, agreed on once by the leader so
    /// every worker downloads the same bytes even if a newer release lands
    /// on GitHub mid-rollout.
    pub assets: BTreeMap<String, ReleaseAsset>,
    /// arch -> blake3 hash of the downloaded binary, filled in as the first
    /// node of each arch reports `Ready`, and used to confirm a restarted
    /// node actually came back on it.
    pub arch_hashes: BTreeMap<String, String>,
    pub per_node: BTreeMap<String, NodePhase>,
    /// Unix seconds.
    pub started_at: i64,
    /// Set once the whole run has stopped, successfully or not.
    pub finished_at: Option<i64>,
    /// Non-empty once the run has been halted, either by a node's failure or
    /// by the operator cancelling it.
    pub failure_reason: Option<String>,
    /// When the node currently `Restarting` was triggered, so the
    /// orchestrator can tell a slow node from one that is never coming back.
    /// One field is enough because only one node restarts at a time.
    pub restart_started_at: Option<i64>,
}

impl UpdateRunState {
    pub fn new(id: String, nodes: impl IntoIterator<Item = String>, started_at: i64) -> Self {
        Self {
            id,
            release_tag: None,
            owner: None,
            repo: None,
            assets: BTreeMap::new(),
            arch_hashes: BTreeMap::new(),
            per_node: nodes.into_iter().map(|n| (n, NodePhase::Idle)).collect(),
            started_at,
            finished_at: None,
            failure_reason: None,
            restart_started_at: None,
        }
    }

    /// Whether the run is still going: not every node has reached a terminal
    /// phase and nothing has failed it outright.
    pub fn in_progress(&self) -> bool {
        self.finished_at.is_none()
    }

    pub fn has_failed(&self) -> bool {
        self.failure_reason.is_some() || self.per_node.values().any(NodePhase::is_failed)
    }

    /// True once every node has reached `Done`.
    pub fn is_complete(&self) -> bool {
        !self.per_node.is_empty() && self.per_node.values().all(|p| *p == NodePhase::Done)
    }

    /// Mark the run halted, recording why. Idempotent: calling it twice keeps
    /// the first reason, since that is the one that actually stopped things.
    pub fn fail(&mut self, reason: impl Into<String>, now: i64) {
        if self.failure_reason.is_none() {
            self.failure_reason = Some(reason.into());
        }
        self.finished_at.get_or_insert(now);
    }

    pub fn finish(&mut self, now: i64) {
        self.finished_at.get_or_insert(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_progression_follows_the_declared_order() {
        let mut phase = NodePhase::Idle;
        let expected = [NodePhase::Fetching];
        for next in expected {
            let advanced = phase.next().expect("has a next phase");
            assert_eq!(advanced, next);
            phase = advanced;
        }
    }

    #[test]
    fn fetching_and_terminal_phases_have_no_declared_next() {
        assert!(NodePhase::Fetching.next().is_none());
        assert!(NodePhase::Done.next().is_none());
        assert!(NodePhase::FetchFailed { reason: "x".into() }.next().is_none());
    }

    #[test]
    fn failed_phases_are_terminal_and_reported_as_failed() {
        let failed = NodePhase::FetchFailed { reason: "no such asset".into() };
        assert!(failed.is_terminal());
        assert!(failed.is_failed());
        assert!(!NodePhase::Fetching.is_failed());
    }

    #[test]
    fn a_run_completes_only_once_every_node_is_done() {
        let mut run = UpdateRunState::new("run-1".into(), ["a".to_string(), "b".to_string()], 0);
        assert!(!run.is_complete());
        run.per_node.insert("a".into(), NodePhase::Done);
        assert!(!run.is_complete());
        run.per_node.insert("b".into(), NodePhase::Done);
        assert!(run.is_complete());
    }

    #[test]
    fn failing_a_run_keeps_the_first_reason() {
        let mut run = UpdateRunState::new("run-1".into(), ["a".to_string()], 0);
        run.fail("build failed on a", 10);
        run.fail("a different reason", 20);
        assert_eq!(run.failure_reason.as_deref(), Some("build failed on a"));
        assert_eq!(run.finished_at, Some(10));
    }

    #[test]
    fn a_node_failure_marks_the_run_as_failed() {
        let mut run = UpdateRunState::new("run-1".into(), ["a".to_string()], 0);
        run.per_node.insert("a".into(), NodePhase::RestartFailed { reason: "never came back".into() });
        assert!(run.has_failed());
    }
}
