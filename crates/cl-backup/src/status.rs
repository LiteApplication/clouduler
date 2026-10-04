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

/// Reserved config name the last webhook-triggered pull's outcome is stored
/// under, mirroring `LAST_EXPORT_CONFIG`. Never exposed through the generic
/// `/api/configs` routes.
pub const LAST_PULL_CONFIG: &str = "_clouduler_git_backup_last_pull";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastPull {
    /// Unix seconds.
    pub at: u64,
    /// The commit that was fetched and accepted, if this pull applied one.
    pub commit: Option<String>,
    /// Set when the fetched commit failed validation: what went wrong, and
    /// the branch the rejected commit was preserved on before the tracked
    /// branch was force-pushed back to a working state.
    pub rejected: Option<RejectedPull>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RejectedPull {
    pub reason: String,
    pub preserved_branch: String,
}

pub fn load_pull(state: &ClusterState) -> Option<LastPull> {
    let entry = state.configs.get(LAST_PULL_CONFIG)?;
    let bytes = entry.decode().ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Returns the commit oid of the most recent successful operation (export or pull).
pub fn last_known_good_commit(state: &ClusterState) -> Option<String> {
    let export = load(state).and_then(|e| {
        if e.error.is_none() {
            e.commit.map(|c| (e.at, c))
        } else {
            None
        }
    });
    let pull = load_pull(state).and_then(|p| {
        if p.rejected.is_none() {
            p.commit.map(|c| (p.at, c))
        } else {
            None
        }
    });
    match (export, pull) {
        (Some((at_e, commit_e)), Some((at_p, commit_p))) => {
            if at_p >= at_e {
                Some(commit_p)
            } else {
                Some(commit_e)
            }
        }
        (Some((_, commit)), None) => Some(commit),
        (None, Some((_, commit))) => Some(commit),
        (None, None) => None,
    }
}

/// The commit a `LastExport` should carry after an export attempt that
/// succeeded: the one it just pushed, or - for a no-op attempt that pushed
/// nothing - the last known good commit already on record, so a no-op never
/// erases the baseline later pulls diff against.
pub fn commit_to_record(pushed: Option<String>, state: &ClusterState) -> Option<String> {
    pushed.or_else(|| last_known_good_commit(state))
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

    #[test]
    fn last_known_good_commit_prefers_most_recent_successful_operation() {
        let mut state = ClusterState::default();
        assert_eq!(last_known_good_commit(&state), None);

        let export = LastExport {
            at: 100,
            summary: "export".into(),
            commit: Some("commit_export".into()),
            error: None,
        };
        state.configs.insert(
            LAST_EXPORT_CONFIG.to_string(),
            ConfigEntry {
                name: LAST_EXPORT_CONFIG.to_string(),
                content: cl_raft::base64_encode(&serde_json::to_vec(&export).unwrap()),
                revision: 1,
                owner: None,
            },
        );
        assert_eq!(
            last_known_good_commit(&state),
            Some("commit_export".to_string())
        );

        // Rejected pull should not overwrite good export commit
        let rejected_pull = LastPull {
            at: 200,
            commit: None,
            rejected: Some(RejectedPull {
                reason: "bad".into(),
                preserved_branch: "rejected/main-bad".into(),
            }),
        };
        state.configs.insert(
            LAST_PULL_CONFIG.to_string(),
            ConfigEntry {
                name: LAST_PULL_CONFIG.to_string(),
                content: cl_raft::base64_encode(&serde_json::to_vec(&rejected_pull).unwrap()),
                revision: 2,
                owner: None,
            },
        );
        assert_eq!(
            last_known_good_commit(&state),
            Some("commit_export".to_string())
        );

        // Successful pull should become the latest
        let good_pull = LastPull {
            at: 300,
            commit: Some("commit_pull".into()),
            rejected: None,
        };
        state.configs.insert(
            LAST_PULL_CONFIG.to_string(),
            ConfigEntry {
                name: LAST_PULL_CONFIG.to_string(),
                content: cl_raft::base64_encode(&serde_json::to_vec(&good_pull).unwrap()),
                revision: 3,
                owner: None,
            },
        );
        assert_eq!(
            last_known_good_commit(&state),
            Some("commit_pull".to_string())
        );
    }

    #[test]
    fn a_no_op_export_keeps_the_last_known_good_commit() {
        let export = LastExport {
            at: 1,
            summary: "add stack web".to_string(),
            commit: Some("deadbeef".to_string()),
            error: None,
        };
        let mut state = ClusterState::default();
        state.configs.insert(
            LAST_EXPORT_CONFIG.to_string(),
            cl_raft::state::ConfigEntry {
                name: LAST_EXPORT_CONFIG.to_string(),
                content: cl_raft::base64_encode(&serde_json::to_vec(&export).expect("encode")),
                revision: 1,
                owner: None,
            },
        );

        assert_eq!(
            commit_to_record(None, &state).as_deref(),
            Some("deadbeef"),
            "a no-op export must not erase the last good commit"
        );
        assert_eq!(
            commit_to_record(Some("cafe".into()), &state).as_deref(),
            Some("cafe")
        );
        assert_eq!(commit_to_record(None, &ClusterState::default()), None);
    }
}
