//! `/api/proxy/remembered-ips`: the client addresses visitors saved at the
//! service login ("remember this IP address"), for an admin to review and
//! revoke.
//!
//! They live in the replicated login-session map, not in configs or secrets,
//! so the git backup never exports them, and `ClusterState::redacted` hides
//! them from everyone but this admin-only route.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use cl_raft::state::REMEMBERED_IP_HOST_PREFIX;
use cl_raft::Request as ClusterRequest;
use serde::Serialize;
use tracing::info;

use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/proxy/remembered-ips", get(list))
        .route("/api/proxy/remembered-ips/{id}", delete(remove))
}

#[derive(Serialize)]
struct RememberedIp {
    /// Opaque; pass it back to delete.
    id: String,
    network: String,
    ip: String,
    username: String,
    groups: Vec<String>,
    /// Unix seconds after which the address no longer skips the login.
    expires_at: i64,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn list(_admin: RequireAdmin, State(state): State<AppState>) -> Json<Vec<RememberedIp>> {
    let now = now();
    let cluster_state = state.cluster.state().await;
    let mut rows: Vec<RememberedIp> = cluster_state
        .proxy_sessions
        .iter()
        .filter(|(_, s)| s.expires_at > now)
        .filter_map(|(key, s)| {
            let rest = s.host.strip_prefix(REMEMBERED_IP_HOST_PREFIX)?;
            let (network, ip) = rest.split_once(':')?;
            Some(RememberedIp {
                id: key.clone(),
                network: network.to_string(),
                ip: ip.to_string(),
                username: s.username.clone(),
                groups: s.groups.clone(),
                expires_at: s.expires_at,
            })
        })
        .collect();
    rows.sort_by(|a, b| (&a.network, &a.ip).cmp(&(&b.network, &b.ip)));
    Json(rows)
}

async fn remove(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    // Only a saved address: this route must not become a way to delete
    // arbitrary login sessions by key.
    let is_remembered = state
        .cluster
        .state()
        .await
        .proxy_sessions
        .get(&id)
        .is_some_and(|s| s.host.starts_with(REMEMBERED_IP_HOST_PREFIX));
    if !is_remembered {
        return Err(ApiError::NotFound(format!("no remembered address {id}")));
    }
    let response = state
        .cluster
        .submit(ClusterRequest::RemoveProxySession { key: id.clone() })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    info!(id, "revoked a remembered ip address");
    Ok(StatusCode::NO_CONTENT)
}
