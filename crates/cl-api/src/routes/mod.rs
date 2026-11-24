pub mod alerts;
pub mod auth;
pub mod auth_keys;
pub mod certs;
pub mod cloudflare;
pub mod cluster;
pub mod config;
pub mod drives;
pub mod git_backup;
pub mod images;
pub mod instances;
pub mod key_distribution;
pub mod links;
pub mod nodes;
pub mod oauth;
pub mod objects;
pub mod oidc;
pub mod proxy;
pub mod redirects;
pub mod registries;
pub mod remembered_ips;
pub mod restic;
pub mod secret_rotation;
pub mod self_update;
pub mod stacks;
pub mod static_routes;
pub mod terraform;
pub mod updates;
pub mod wireguard;

use axum::Router;

use crate::error::ApiError;
use crate::server::AppState;

pub(crate) fn check_cloudflare_proxy(
    cfg: &cl_config::ClusterConfig,
    network: &str,
    requested: bool,
) -> Result<(), ApiError> {
    if requested
        && !cfg
            .networks
            .get(network)
            .is_some_and(|n| n.kind == cl_config::NetworkKind::Public)
    {
        return Err(ApiError::BadRequest(format!(
            "cloudflare_proxy needs a public network, and {network} is not one"
        )));
    }
    Ok(())
}

/// Redirects and static routes name a node's own address in DNS, and a node's
/// address on a managed network is only reachable through that node, so they
/// cannot live there (a stack route can: it publishes the container's address).
pub(crate) fn check_not_managed(
    cfg: &cl_config::ClusterConfig,
    network: &str,
    what: &str,
) -> Result<(), ApiError> {
    if cfg.is_managed_network(network) {
        return Err(ApiError::BadRequest(format!(
            "{network} is a managed network: a node's address there is only reachable through that node, so {what} cannot be published on it; use a stack route (x-clouduler.proxy), which publishes the container's own address, or another network"
        )));
    }
    Ok(())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(alerts::router())
        .merge(auth::router())
        .merge(auth_keys::router())
        .merge(remembered_ips::router())
        .merge(cluster::router())
        .merge(links::router())
        .merge(nodes::router())
        .merge(stacks::router())
        .merge(terraform::router())
        .merge(instances::router())
        .merge(objects::router())
        .merge(secret_rotation::router())
        .merge(drives::router())
        .merge(git_backup::router())
        .merge(images::router())
        .merge(key_distribution::router())
        .merge(config::router())
        .merge(cloudflare::router())
        .merge(oauth::router())
        .merge(crate::mcp::router())
        .merge(oidc::router())
        .merge(proxy::router())
        .merge(certs::router())
        .merge(redirects::router())
        .merge(static_routes::router())
        .merge(self_update::router())
        .merge(registries::router())
        .merge(restic::router())
        .merge(updates::router())
        .merge(wireguard::router())
}

#[cfg(test)]
mod managed_tests {
    use super::*;

    #[test]
    fn a_redirect_or_static_route_is_refused_on_a_managed_network() {
        let mut cfg =
            cl_config::ClusterConfig::parse(include_str!("../../../../examples/cluster.yaml"))
                .expect("example parses");
        cfg.networks.insert(
            "mgd".into(),
            serde_yaml::from_str("{ type: private, subnet: 192.168.32.0/24, managed: true }")
                .unwrap(),
        );
        check_not_managed(&cfg, "lan", "a redirect").expect("an ordinary network is fine");
        let err = check_not_managed(&cfg, "mgd", "a static route")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("managed network") && err.contains("static route"),
            "{err}"
        );
    }
}
