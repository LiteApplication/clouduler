//! The HTTP API and the web ui it serves. See docs/api.md for the contract.

pub mod alerts;
pub mod auth;
pub mod auto_update;
pub mod cloudflare;
pub mod container_router;
pub mod docker_stats;
pub mod error;
pub mod internal_peer;
pub mod live_usage;
pub mod mcp;
pub mod migration_problem;
pub mod node_logs;
pub mod node_metrics;
pub mod nudges;
pub mod oidc;
pub mod redirects;
pub mod routes;
pub mod secrets;
pub mod server;
pub mod static_routes;
pub mod stream;
pub mod ui;
pub mod volume_ops;
pub mod wireguard;

pub use error::ApiError;
pub use nudges::Nudges;
pub use server::{ApiServer, ApiServerOptions, AppState};

pub(crate) fn encode_json_config<T: serde::Serialize>(value: &T) -> String {
    let raw =
        serde_json::to_vec(value).expect("plain data structs with string keys always serialize");
    cl_raft::base64_encode(&raw)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod encode_tests {
    #[test]
    fn json_config_is_base64_of_the_json() {
        let encoded = super::encode_json_config(&vec!["a", "b"]);
        assert_eq!(cl_raft::base64_decode(&encoded).unwrap(), br#"["a","b"]"#);
    }
}
