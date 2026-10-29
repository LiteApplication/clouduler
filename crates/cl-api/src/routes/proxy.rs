//! `GET /api/proxy/routes`: what this node's reverse proxy is publishing.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use cl_proxy::Route;
use serde::Serialize;

use crate::auth::Identity;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/proxy/routes", get(list))
}

#[derive(Serialize)]
struct RouteRow {
    host: String,
    path: Option<String>,
    /// `service`, `redirect`, `static` or `dashboard`.
    kind: &'static str,
    stack: Option<String>,
    service: Option<String>,
    /// First backend's port; `None` for a redirect or a route with no
    /// backend resolved yet.
    port: Option<u16>,
    network: Option<String>,
    tls: bool,
    websockets: bool,
    /// The host is the apex of its network's domain (written `@` in config).
    apex: bool,
    /// Unix seconds the certificate covering this host expires, when one is
    /// held.
    cert_not_after: Option<i64>,
    health: Option<RouteHealth>,
}

#[derive(Serialize)]
struct RouteHealth {
    /// The node whose proxy ran the check; every node probes on its own.
    node: String,
    checked_at: String,
    healthy: bool,
    error: Option<String>,
}

/// Where a route came from, and for a service route its stack and service,
/// decoded from the label the daemon gives it (`Route::instance`).
fn classify(instance: &str) -> (&'static str, Option<String>, Option<String>) {
    if instance == "clouduler.dashboard" {
        ("dashboard", None, None)
    } else if instance.starts_with("clouduler.redirect.") {
        ("redirect", None, None)
    } else if instance.starts_with("clouduler.static.") {
        ("static", None, None)
    } else {
        match instance.split_once('.') {
            Some((stack, service)) => (
                "service",
                Some(stack.to_string()),
                Some(service.to_string()),
            ),
            None => ("service", Some(instance.to_string()), None),
        }
    }
}

/// The expiry of the certificate that would serve `host`: its own, else the
/// wildcard one for its parent domain.
fn cert_expiry(state: &cl_raft::ClusterState, host: &str) -> Option<i64> {
    state
        .certificates
        .get(host)
        .or_else(|| {
            let (_, parent) = host.split_once('.')?;
            state.certificates.get(&format!("*.{parent}"))
        })
        .map(|c| c.not_after)
}

fn row(
    route: &Route,
    state: &AppState,
    cluster_state: &cl_raft::ClusterState,
    apexes: &std::collections::HashSet<String>,
) -> RouteRow {
    let (kind, stack, service) = classify(&route.instance);
    let health = state
        .proxy_health
        .get(&route.instance)
        .map(|h| RouteHealth {
            node: state.node_name.clone(),
            checked_at: humantime::format_rfc3339_seconds(h.checked_at).to_string(),
            healthy: h.healthy(),
            error: h.error,
        });
    RouteRow {
        host: route.host.clone(),
        path: route.path_prefix.clone(),
        kind,
        stack,
        service,
        port: route.backends().first().map(|b| b.port()),
        network: route.options.network.clone(),
        tls: route.tls,
        websockets: route.options.websockets,
        apex: apexes.contains(&route.host),
        cert_not_after: if route.tls {
            cert_expiry(cluster_state, &route.host)
        } else {
            None
        },
        health,
    }
}

/// Empty while the proxy is off on this node.
async fn list(_identity: Identity, State(state): State<AppState>) -> Json<Vec<RouteRow>> {
    let Some(table) = state.proxy_routes.get() else {
        return Json(Vec::new());
    };
    let cluster_state = state.cluster.state().await;
    let apexes: std::collections::HashSet<String> = state
        .cluster_config
        .read()
        .await
        .networks
        .values()
        .filter_map(|n| n.domain.clone())
        .collect();
    Json(
        table
            .snapshot()
            .iter()
            .map(|r| row(r, &state, &cluster_state, &apexes))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_labels_decode_to_a_kind() {
        assert_eq!(classify("clouduler.dashboard").0, "dashboard");
        assert_eq!(classify("clouduler.redirect.abc").0, "redirect");
        assert_eq!(classify("clouduler.static.old.example.com").0, "static");
        assert_eq!(
            classify("nextcloud.web"),
            ("service", Some("nextcloud".into()), Some("web".into()))
        );
    }
}
