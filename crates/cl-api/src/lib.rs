//! The HTTP API and the web ui it serves. See docs/api.md for the contract.

pub mod auth;
pub mod config_edit;
pub mod docker_stats;
pub mod error;
pub mod routes;
pub mod secrets;
pub mod server;
pub mod stream;
pub mod ui;

pub use error::ApiError;
pub use server::{ApiServer, ApiServerOptions, AppState};

#[cfg(test)]
mod tests;
