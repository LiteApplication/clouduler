//! `/api/cloudflare/token`: setting the Cloudflare API token from the web ui.
//!
//! The token is write-only over the api. It is sealed and stored as a
//! cluster secret (`cl_api::cloudflare::CLOUDFLARE_TOKEN_SECRET`), the same
//! pattern the wireguard config import route uses
//! (`crate::routes::wireguard::put_config`) -- only its presence is ever
//! reported back, there is no route that returns the value, so a stolen
//! session cannot read out a token someone else installed.
//!
//! Replicated through raft rather than written to a file on one node, so
//! every node can open it with the cluster key and certificate issuance or
//! DNS publishing keeps working after a failover to a different leader.

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
    /// True once a token is stored, which is what the ui keys the "you cannot
    /// see or change this here" state off.
    configured: bool,
}

async fn token_status(_identity: Identity, State(state): State<AppState>) -> Json<TokenStatus> {
    let cluster_state = state.cluster.state().await;
    Json(TokenStatus {
        configured: cluster_state.secrets.contains_key(CLOUDFLARE_TOKEN_SECRET),
    })
}

#[derive(Deserialize)]
struct SetTokenRequest {
    token: String,
}

/// Seals and stores the token, replacing whatever was there before: rotation
/// is safe now that there is exactly one copy of it, replicated to every
/// node, rather than a file an operator would otherwise have to update by
/// hand on each manager.
async fn set_token(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<SetTokenRequest>,
) -> Result<StatusCode, ApiError> {
    let token = body.token.trim().to_string();
    if token.is_empty() {
        return Err(ApiError::BadRequest("the token is empty".into()));
    }
    {
        let config = state.cluster_config.read().await;
        if config.cloudflare.is_none() {
            return Err(ApiError::Conflict("cloudflare is not enabled in the config".into()));
        }
    }

    let key_guard = state.key.read().await;
    let key = key_guard
        .as_ref()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;
    let sealed = Sealed::seal(key, CLOUDFLARE_TOKEN_SECRET, token.as_bytes()).map_err(ApiError::internal)?;
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
