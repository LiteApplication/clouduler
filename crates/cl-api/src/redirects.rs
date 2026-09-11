//! UI-managed proxy redirects: rules that answer a host with a `Location`
//! instead of proxying to a backend. Deliberately not part of `cluster.yaml`
//! — these are expected to change often enough that hand-editing the config
//! file for each one would be annoying, so they live in replicated state
//! under a reserved config name instead, the same way [`crate::auth::keys`]
//! stores api keys.

use serde::{Deserialize, Serialize};

use cl_raft::{base64_encode, ClusterState};

/// Reserved config name holding the redirect list. Never exposed as a
/// regular config: routes that list or serve user configs must filter it out.
pub const REDIRECTS_CONFIG: &str = "_clouduler_redirects";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedirectRule {
    pub id: String,
    /// Bare label gets the network's domain appended, same as a stack route.
    pub host: String,
    /// Network the hostname is qualified against and gated by.
    pub network: String,
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Destination URL, e.g. `https://new.example.com`.
    pub target: String,
    /// Append the request's original path and query to `target`.
    #[serde(default)]
    pub preserve_path: bool,
    /// 301, 302, 307 or 308.
    pub status: u16,
}

impl RedirectRule {
    pub fn valid_status(status: u16) -> bool {
        matches!(status, 301 | 302 | 307 | 308)
    }
}

/// A short random id for a new rule, in the same spirit as api key ids: not a
/// secret, just unique enough to reference and delete one entry.
pub fn new_id() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut buf = [0u8; 12];
    rand::fill(&mut buf);
    buf.iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RedirectStore {
    pub rules: Vec<RedirectRule>,
}

impl RedirectStore {
    /// Read the store out of applied cluster state. Empty if none exist yet.
    pub fn load(state: &ClusterState) -> Self {
        let Some(entry) = state.configs.get(REDIRECTS_CONFIG) else {
            return Self::default();
        };
        let Ok(raw) = entry.decode() else {
            return Self::default();
        };
        serde_json::from_slice(&raw).unwrap_or_default()
    }

    /// Base64 content ready to submit via `Request::PutConfig`.
    pub fn encode(&self) -> String {
        let raw = serde_json::to_vec(self).expect("RedirectStore always serializes");
        base64_encode(&raw)
    }

    pub fn find(&self, id: &str) -> Option<&RedirectRule> {
        self.rules.iter().find(|r| r.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str) -> RedirectRule {
        RedirectRule {
            id: id.to_string(),
            host: "old".to_string(),
            network: "wan".to_string(),
            path_prefix: None,
            target: "https://new.example.com".to_string(),
            preserve_path: false,
            status: 301,
        }
    }

    #[test]
    fn round_trips_through_encode_and_decode() {
        let store = RedirectStore {
            rules: vec![rule("a")],
        };
        let raw = cl_raft::base64_decode(&store.encode()).expect("valid base64");
        let decoded: RedirectStore = serde_json::from_slice(&raw).expect("valid json");
        assert_eq!(decoded.rules, store.rules);
    }

    #[test]
    fn missing_config_loads_as_empty() {
        let state = ClusterState::default();
        assert!(RedirectStore::load(&state).rules.is_empty());
    }

    #[test]
    fn only_the_expected_statuses_are_valid() {
        assert!(RedirectRule::valid_status(301));
        assert!(RedirectRule::valid_status(302));
        assert!(RedirectRule::valid_status(307));
        assert!(RedirectRule::valid_status(308));
        assert!(!RedirectRule::valid_status(200));
        assert!(!RedirectRule::valid_status(303));
    }
}
