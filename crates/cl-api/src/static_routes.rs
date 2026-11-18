//! UI-managed static/external proxy backends: routes for a service clouduler does not run, backed by a fixed
//! `host:port`. Not in `cluster.yaml`: like a redirect ([`crate::redirects`]) they live in replicated state under a
//! reserved config name, so they change without a restart.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use cl_config::{ByteSize, Middleware};
use cl_raft::ClusterState;

pub use crate::redirects::new_id;

/// Never exposed as a regular config: routes that list or serve user configs must filter it out.
pub const STATIC_ROUTES_CONFIG: &str = "_clouduler_static_routes";

/// Resolved by DNS on every reconcile pass, so a hostname backend picks up a changed address.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StaticRouteRule {
    pub id: String,
    pub host: String,
    pub network: String,
    /// `None` publishes every node, like any other static route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// A hostname is resolved fresh each reconcile pass; every address becomes a backend, round robined.
    pub backend: String,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default = "default_true")]
    pub tls: bool,
    #[serde(default)]
    pub middlewares: Vec<Middleware>,
    #[serde(default)]
    pub max_body_size: Option<ByteSize>,
    #[serde(default = "default_true")]
    pub websockets: bool,
    #[serde(default)]
    pub preserve_host: bool,
    #[serde(default, with = "humantime_serde::option")]
    pub response_timeout: Option<Duration>,
    #[serde(default)]
    pub cloudflare_proxy: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StaticRouteStore {
    pub routes: Vec<StaticRouteRule>,
}

impl StaticRouteStore {
    pub fn load(state: &ClusterState) -> Self {
        let Some(entry) = state.configs.get(STATIC_ROUTES_CONFIG) else {
            return Self::default();
        };
        let Ok(raw) = entry.decode() else {
            return Self::default();
        };
        serde_json::from_slice(&raw).unwrap_or_default()
    }

    pub fn encode(&self) -> String {
        crate::encode_json_config(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str) -> StaticRouteRule {
        StaticRouteRule {
            id: id.to_string(),
            host: "printer".to_string(),
            network: "lan".to_string(),
            node: None,
            backend: "192.168.1.50:9100".to_string(),
            path_prefix: None,
            tls: true,
            middlewares: Vec::new(),
            max_body_size: None,
            websockets: true,
            preserve_host: false,
            response_timeout: None,
            cloudflare_proxy: false,
        }
    }

    #[test]
    fn round_trips_through_encode_and_decode() {
        let store = StaticRouteStore {
            routes: vec![rule("a")],
        };
        let raw = cl_raft::base64_decode(&store.encode()).expect("valid base64");
        let decoded: StaticRouteStore = serde_json::from_slice(&raw).expect("valid json");
        assert_eq!(decoded.routes, store.routes);
    }

    #[test]
    fn missing_config_loads_as_empty() {
        let state = ClusterState::default();
        assert!(StaticRouteStore::load(&state).routes.is_empty());
    }
}
