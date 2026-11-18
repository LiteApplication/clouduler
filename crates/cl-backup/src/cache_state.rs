//! Persists `watch_git_backup`'s change-detection state (`Snapshot` and
//! `SecretCache`) across restarts and leader handoffs. Starting empty would
//! describe every object as newly added and re-encrypt every secret; `age` is
//! non-deterministic, so that yields a spurious commit touching every secret.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::snapshot::Snapshot;
use crate::tree::SecretCache;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheState {
    pub last_snapshot: Option<Snapshot>,
    pub secret_cache: SecretCache,
}

impl CacheState {
    /// Missing or unreadable/corrupt is treated the same as "never
    /// exported": the next tick just re-derives everything.
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        std::fs::write(path, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache.json");
        let state = CacheState::default();
        state.save(&path).expect("save");
        assert!(CacheState::load(&path).last_snapshot.is_none());
    }

    #[test]
    fn missing_file_loads_as_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = CacheState::load(&dir.path().join("nope.json"));
        assert!(state.last_snapshot.is_none());
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache.json");

        let state = CacheState {
            last_snapshot: Some(Snapshot {
                cluster_config_generation: Some(3),
                ..Default::default()
            }),
            ..Default::default()
        };
        state.save(&path).expect("save");

        let loaded = CacheState::load(&path);
        assert_eq!(loaded.last_snapshot, state.last_snapshot);
    }
}
