//! `/api/cloudflare/token`: setting the Cloudflare API token from the web ui.
//!
//! Write-only: sealed and stored as a cluster secret (`cl_api::cloudflare::CLOUDFLARE_TOKEN_SECRET`), like the wireguard
//! config import (`crate::routes::wireguard::put_config`); only its presence is reported back. Replicated through raft so
//! certificate issuance and DNS publishing survive a leader failover.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use cl_raft::Request as ClusterRequest;
use cl_secret::Sealed;
use serde::{Deserialize, Serialize};

use crate::auth::{Identity, RequireAdmin};
use crate::cloudflare::CLOUDFLARE_TOKEN_SECRET;
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/cloudflare/token", get(token_status).put(set_token))
}

#[derive(Serialize)]
struct TokenStatus {
    /// The ui keys its "cannot see or change this here" state off it.
    configured: bool,
}

async fn token_status(_identity: Identity, State(state): State<AppState>) -> Json<TokenStatus> {
    let cluster_state = state.cluster.snapshot().await;
    Json(TokenStatus {
        configured: cluster_state.secrets.contains_key(CLOUDFLARE_TOKEN_SECRET),
    })
}

#[derive(Deserialize)]
struct SetTokenRequest {
    token: String,
}

/// Replacing is safe rotation: there is exactly one copy, replicated to every node. Settable whether or not `cloudflare`
/// is enabled, so a token can be staged before flipping DNS management on.
async fn set_token(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<SetTokenRequest>,
) -> Result<StatusCode, ApiError> {
    let token = body.token.trim().to_string();
    if token.is_empty() {
        return Err(ApiError::BadRequest("the token is empty".into()));
    }

    let key_guard = state.key.read().await;
    let key = key_guard
        .as_ref()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;
    let sealed =
        Sealed::seal(key, CLOUDFLARE_TOKEN_SECRET, token.as_bytes()).map_err(ApiError::internal)?;
    let key_fingerprint = key.fingerprint();
    drop(key_guard);

    let response = state
        .cluster
        .submit(ClusterRequest::PutSecret {
            name: CLOUDFLARE_TOKEN_SECRET.to_string(),
            sealed: sealed.blob,
            key_fingerprint,
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }
    Ok(StatusCode::NO_CONTENT)
}
