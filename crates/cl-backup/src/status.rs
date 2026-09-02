//! The leader's last git export attempt, for the web ui.
//!
//! Stored as json in the reserved config object `_clouduler_git_backup_last_export`,
//! the same trick `cl-update`'s `UpdateRunState` uses: it rides the existing
//! config CAS plumbing and `/api/events` SSE stream for free.

use cl_raft::state::ClusterState;
use serde::{Deserialize, Serialize};

/// Reserved config name the last-export status is stored under. Never
/// exposed through the generic `/api/configs` routes.
pub const LAST_EXPORT_CONFIG: &str = "_clouduler_git_backup_last_export";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastExport {
    /// Unix seconds.
    pub at: u64,
    /// One-line description of what changed, as built by `snapshot::commit_message`.
    pub summary: String,
    /// The git commit that was pushed, if this attempt produced one (a
    /// no-op tick, where nothing tracked had changed, has neither this nor
    /// an error).
    pub commit: Option<String>,
    /// Set when the export attempt failed - a bad remote, auth rejected, a
    /// secret this node's key can't open, etc. The task keeps retrying on
    /// its normal interval either way.
    pub error: Option<String>,
}

pub fn load(state: &ClusterState) -> Option<LastExport> {
    let entry = state.configs.get(LAST_EXPORT_CONFIG)?;
    let bytes = entry.decode().ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::ConfigEntry;

    #[test]
    fn round_trips_through_a_config_entry() {
        let export = LastExport {
            at: 12345,
            summary: "add stack web".to_string(),
            commit: Some("deadbeef".to_string()),
            error: None,
        };
        let content = cl_raft::base64_encode(&serde_json::to_vec(&export).expect("encode"));

        let mut state = ClusterState::default();
        state.configs.insert(
            LAST_EXPORT_CONFIG.to_string(),
            ConfigEntry {
                name: LAST_EXPORT_CONFIG.to_string(),
                content,
                revision: 1,
                owner: None,
            },
        );

        assert_eq!(load(&state), Some(export));
    }

    #[test]
    fn absent_when_never_exported() {
        let state = ClusterState::default();
        assert_eq!(load(&state), None);
    }
}
