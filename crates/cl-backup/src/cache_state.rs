//! Persists `watch_git_backup`'s in-memory change-detection state across
//! restarts and leader handoffs.
//!
//! Both `snapshot::Snapshot` (what the last successful export actually
//! contained) and `tree::SecretCache` (each secret's last-encrypted
//! ciphertext) are kept on disk. A restart or a leadership change starts the
//! loop fresh with `None`/empty, which would make the next export's commit
//! message describe every tracked object as newly added (diffed against
//! `Snapshot::default()`) and re-encrypt every secret from scratch - `age` is
//! non-deterministic, so the fresh ciphertext differs from what's already
//! committed even though the plaintext never changed, producing a spurious
//! commit that touches every secret file.

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
        let bytes = serde_json::to_vec(self).expect("CacheState always serializes");
        std::fs::write(path, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
