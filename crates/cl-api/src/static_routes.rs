//! UI-managed static/external proxy backends: routes for a service clouduler
//! does not run itself, backed by a fixed `host:port` instead of a stack's
//! containers. Deliberately not part of `cluster.yaml` — like a redirect (see
//! [`crate::redirects`]), one of these can be added or edited without a
//! restart, since it lives in replicated state under a reserved config name
//! rather than the file every daemon has to be restarted to pick up.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use cl_config::{ByteSize, Middleware};
use cl_raft::{base64_encode, ClusterState};

pub use crate::redirects::new_id;

/// Reserved config name holding the static route list. Never exposed as a
/// regular config: routes that list or serve user configs must filter it out.
pub const STATIC_ROUTES_CONFIG: &str = "_clouduler_static_routes";

/// A proxy route whose backend is a fixed address instead of a stack's
/// containers. Resolved by DNS on every reconcile pass, so a hostname backend
/// picks up a changed address without the store being touched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StaticRouteRule {
    pub id: String,
    /// Bare label gets the network's domain appended, same as a stack route.
    pub host: String,
    /// Declared network this route is published on.
    pub network: String,
    /// `host:port` of the backend. A hostname is resolved fresh on every
    /// reconcile pass; every address it resolves to becomes a backend, round
    /// robined like a multi replica service.
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
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StaticRouteStore {
    pub routes: Vec<StaticRouteRule>,
}

impl StaticRouteStore {
    /// Read the store out of applied cluster state. Empty if none exist yet.
    pub fn load(state: &ClusterState) -> Self {
        let Some(entry) = state.configs.get(STATIC_ROUTES_CONFIG) else {
            return Self::default();
        };
        let Ok(raw) = entry.decode() else {
            return Self::default();
        };
        serde_json::from_slice(&raw).unwrap_or_default()
    }

    /// Base64 content ready to submit via `Request::PutConfig`.
    pub fn encode(&self) -> String {
        let raw = serde_json::to_vec(self).expect("StaticRouteStore always serializes");
        base64_encode(&raw)
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
            backend: "192.168.1.50:9100".to_string(),
            path_prefix: None,
            tls: true,
            middlewares: Vec::new(),
            max_body_size: None,
            websockets: true,
            preserve_host: false,
            response_timeout: None,
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
