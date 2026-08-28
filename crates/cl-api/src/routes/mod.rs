//! One module per section of docs/api.md. Handlers stay thin: parse,
//! authorize, call into the cluster or docker, serialize.

pub mod auth;
pub mod auth_keys;
pub mod certs;
pub mod cloudflare;
pub mod cluster;
pub mod config;
pub mod drives;
pub mod instances;
pub mod key_distribution;
pub mod nodes;
pub mod objects;
pub mod oidc;
pub mod redirects;
pub mod registries;
pub mod self_update;
pub mod stacks;
pub mod static_routes;
pub mod updates;
pub mod wireguard;

use axum::Router;

use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(auth::router())
        .merge(auth_keys::router())
        .merge(cluster::router())
        .merge(nodes::router())
        .merge(stacks::router())
        .merge(instances::router())
        .merge(objects::router())
        .merge(drives::router())
        .merge(config::router())
        .merge(cloudflare::router())
        .merge(oidc::router())
        .merge(certs::router())
        .merge(redirects::router())
        .merge(static_routes::router())
        .merge(self_update::router())
        .merge(registries::router())
        .merge(updates::router())
        .merge(wireguard::router())
}
