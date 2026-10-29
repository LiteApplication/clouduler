//! The HTTP API and the web ui it serves. See docs/api.md for the contract.

pub mod auth;
pub mod cloudflare;
pub mod config_edit;
pub mod docker_stats;
pub mod error;
pub mod internal_peer;
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

#[cfg(test)]
mod tests;
