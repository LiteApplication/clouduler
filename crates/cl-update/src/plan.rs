//! The progress object for one self-update run.
//!
//! This is exactly what gets stored, as json, in the reserved config object
//! `_clouduler_self_update_run`: it rides the existing config CAS plumbing and
//! the existing `/api/events` SSE stream for free, so no new raft entry type
//! is needed for it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Reserved config name the run state is stored under. Never exposed through
/// the generic `/api/configs` routes.
pub const RUN_CONFIG: &str = "_clouduler_self_update_run";

/// Reserved secret name the sealed ssh deploy key is stored under. Never
/// exposed through the generic `/api/secrets` routes.
pub const DEPLOY_KEY_SECRET: &str = "_clouduler_self_update_deploy_key";

/// One node's progress through a run, in the order it must be crossed.
///
/// Forward-only, like `cl_raft::state::MigrationPhase`: a stale message must
/// never rewind a node's phase, since two workers racing to report the same
/// step would otherwise be able to bounce it back and forth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodePhase {
    Idle,
    AssignedBuild,
    Cloning,
    Building,
    Built { sha256: String },
    BuildFailed { reason: String },
    AwaitingArtifact,
    Fetching,
    Ready,
    Restarting,
    Done,
    RestartFailed { reason: String },
}

impl NodePhase {
    /// The phase this one may advance to on success. `None` for a phase that
    /// is either terminal or whose next step depends on which branch a node
    /// took (build vs. fetch), which the caller decides rather than this enum.
    pub fn next(&self) -> Option<NodePhase> {
        use NodePhase::*;
        match self {
            Idle => Some(AssignedBuild),
            AssignedBuild => Some(Cloning),
            Cloning => Some(Building),
            AwaitingArtifact => Some(Fetching),
            Fetching => Some(Ready),
            Ready => Some(Restarting),
            Restarting => Some(Done),
            // Building's success path ends at `Built{sha256}`, which carries
            // data the plain enum ordering can't express as "next".
            Building | Built { .. } | BuildFailed { .. } | Done | RestartFailed { .. } => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            NodePhase::Done | NodePhase::BuildFailed { .. } | NodePhase::RestartFailed { .. }
        )
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, NodePhase::BuildFailed { .. } | NodePhase::RestartFailed { .. })
    }
}

/// Everything the orchestrator and every worker agree on for one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateRunState {
    /// Unique per run, so a worker that missed the start of a run (restarted
    /// mid-flight) can tell "still the run I was doing" from "a new one".
    pub id: String,
    /// Resolved commit sha, once at least one builder has cloned.
    pub target_commit: Option<String>,
    /// arch -> node name doing the build for that arch.
    pub arch_builders: BTreeMap<String, String>,
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
            target_commit: None,
            arch_builders: BTreeMap::new(),
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
        let expected = [
            NodePhase::AssignedBuild,
            NodePhase::Cloning,
            NodePhase::Building,
        ];
        for next in expected {
            let advanced = phase.next().expect("has a next phase");
            assert_eq!(advanced, next);
            phase = advanced;
        }
    }

    #[test]
    fn built_and_terminal_phases_have_no_declared_next() {
        assert!(NodePhase::Built { sha256: "abc".into() }.next().is_none());
        assert!(NodePhase::Done.next().is_none());
        assert!(NodePhase::BuildFailed { reason: "x".into() }.next().is_none());
    }

    #[test]
    fn failed_phases_are_terminal_and_reported_as_failed() {
        let failed = NodePhase::BuildFailed { reason: "no such branch".into() };
        assert!(failed.is_terminal());
        assert!(failed.is_failed());
        assert!(!NodePhase::Building.is_failed());
    }

    #[test]
    fn a_run_completes_only_once_every_node_is_done() {
        let mut run = UpdateRunState::new(
            "run-1".into(),
            ["a".to_string(), "b".to_string()],
            0,
        );
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
        run.per_node.insert(
            "a".into(),
            NodePhase::RestartFailed { reason: "never came back".into() },
        );
        assert!(run.has_failed());
    }
}
