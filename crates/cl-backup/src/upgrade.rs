//! The "this cluster was upgraded" record the git export keeps in `.clouduler-version.json`.
//!
//! It is derived from the last self-update run in raft (the reserved config
//! `_clouduler_self_update_run`), so it is replicated state: whichever node leads the export
//! reproduces the same file, and a leader change cannot lose or duplicate the upgrade commit.
//! Only a run that finished cleanly counts; a failed or cancelled run leaves nothing behind.

use cl_raft::state::ClusterState;
use serde::{Deserialize, Serialize};

/// The reserved config the run state lives in (`cl_update::plan::RUN_CONFIG`; this crate cannot
/// depend on `cl-update`).
const RUN_CONFIG: &str = "_clouduler_self_update_run";

/// Root-level file recording the last successful upgrade (`{"version": "...", "upgraded_at": unix}`).
/// Bookkeeping like the revision files: a pull ignores it and a rollback keeps the current one.
pub const VERSION_PATH: &str = ".clouduler-version.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeRecord {
    pub version: String,
    pub upgraded_at: u64,
}

impl UpgradeRecord {
    pub fn to_json(&self) -> Vec<u8> {
        format!(
            "{:#}",
            serde_json::json!({"version": self.version, "upgraded_at": self.upgraded_at})
        )
        .into_bytes()
    }
}

/// The subset of `cl_update::plan::UpdateRunState` needed here; unknown fields are ignored.
#[derive(Deserialize)]
struct RunView {
    #[serde(default)]
    release_tag: Option<String>,
    #[serde(default)]
    target_tag: Option<String>,
    #[serde(default)]
    per_node: std::collections::BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    finished_at: Option<i64>,
    #[serde(default)]
    failure_reason: Option<String>,
}

/// The last self-update run, if it finished with no failure, every node reached `done`, and the
/// cluster is not in emergency mode.
pub fn successful_upgrade(state: &ClusterState) -> Option<UpgradeRecord> {
    if state.in_emergency() {
        return None;
    }
    let raw = state.configs.get(RUN_CONFIG)?.decode().ok()?;
    let run: RunView = serde_json::from_slice(&raw).ok()?;
    let finished_at = run.finished_at?;
    if run.failure_reason.is_some() || run.per_node.is_empty() {
        return None;
    }
    if !run
        .per_node
        .values()
        .all(|phase| phase.as_str() == Some("done"))
    {
        return None;
    }
    let version = run
        .release_tag
        .or(run.target_tag)
        .filter(|tag| !tag.is_empty())?;
    Some(UpgradeRecord {
        version,
        upgraded_at: u64::try_from(finished_at).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::ConfigEntry;

    fn state_with_run(run: serde_json::Value) -> ClusterState {
        let mut state = ClusterState::default();
        state.configs.insert(
            RUN_CONFIG.to_string(),
            ConfigEntry {
                name: RUN_CONFIG.to_string(),
                content: cl_raft::state::base64_encode(&serde_json::to_vec(&run).unwrap()),
                revision: 1,
                owner: None,
            },
        );
        state
    }

    fn run(finished: Option<i64>, failure: Option<&str>, phase: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "r", "release_tag": "v0.18.0", "assets": {}, "arch_hashes": {},
            "per_node": {"a": "done", "b": phase},
            "started_at": 100, "finished_at": finished, "failure_reason": failure,
            "restart_started_at": null
        })
    }

    #[test]
    fn a_clean_finished_run_is_recorded() {
        let rec = successful_upgrade(&state_with_run(run(Some(200), None, "done"))).unwrap();
        assert_eq!(rec.version, "v0.18.0");
        assert_eq!(rec.upgraded_at, 200);
        let json: serde_json::Value = serde_json::from_slice(&rec.to_json()).unwrap();
        assert_eq!(json["version"], "v0.18.0");
        assert_eq!(json["upgraded_at"], 200);
    }

    #[test]
    fn failed_unfinished_or_partial_runs_leave_nothing() {
        assert!(successful_upgrade(&state_with_run(run(None, None, "done"))).is_none());
        assert!(
            successful_upgrade(&state_with_run(run(Some(200), Some("cancelled"), "done")))
                .is_none()
        );
        assert!(successful_upgrade(&state_with_run(run(Some(200), None, "restarting"))).is_none());
        assert!(successful_upgrade(&state_with_run(run(Some(200), None, "idle"))).is_none());
        assert!(successful_upgrade(&ClusterState::default()).is_none());
    }

    #[test]
    fn emergency_mode_suppresses_the_record() {
        let mut state = state_with_run(run(Some(200), None, "done"));
        state.emergency_stacks.insert("web".to_string());
        assert!(successful_upgrade(&state).is_none());
    }
}
