//! `/api/oidc/client-secret`: setting the OIDC client secret from the web ui.
//!
//! Write-only: sealed and stored as a cluster secret (`cl_api::oidc::OIDC_CLIENT_SECRET`), like the Cloudflare token
//! route; only its presence is reported back, so a stolen session cannot read out an installed secret. Replicated
//! through raft so every node can open it and login survives a leader failover.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use cl_raft::Request as ClusterRequest;
use cl_secret::Sealed;
use serde::{Deserialize, Serialize};

use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::oidc::{OIDC_CLIENT_SECRET, PROXY_OIDC_CLIENT_SECRET};
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/oidc/client-secret",
            get(secret_status).put(set_secret),
        )
        .route(
            "/api/proxy-auth/client-secret",
            get(proxy_secret_status).put(set_proxy_secret),
        )
}

#[derive(Serialize)]
struct SecretStatus {
    /// The ui keys its "cannot see or change this here" state off it.
    configured: bool,
}

async fn secret_status(_identity: Identity, State(state): State<AppState>) -> Json<SecretStatus> {
    status_of(&state, OIDC_CLIENT_SECRET).await
}

async fn proxy_secret_status(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<SecretStatus> {
    status_of(&state, PROXY_OIDC_CLIENT_SECRET).await
}

async fn status_of(state: &AppState, name: &str) -> Json<SecretStatus> {
    let configured = state.cluster.read(|s| s.secrets.contains_key(name)).await;
    Json(SecretStatus { configured })
}

#[derive(Deserialize)]
struct SetSecretRequest {
    client_secret: String,
}

/// Replacing is safe rotation: there is exactly one copy, replicated to every node. Settable whether or not `oidc` is
/// enabled, so a secret can be staged before flipping OIDC on.
async fn set_secret(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<SetSecretRequest>,
) -> Result<StatusCode, ApiError> {
    store_secret(&state, OIDC_CLIENT_SECRET, body).await
}

async fn set_proxy_secret(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<SetSecretRequest>,
) -> Result<StatusCode, ApiError> {
    store_secret(&state, PROXY_OIDC_CLIENT_SECRET, body).await
}

async fn store_secret(
    state: &AppState,
    name: &str,
    body: SetSecretRequest,
) -> Result<StatusCode, ApiError> {
    let secret = body.client_secret.trim().to_string();
    if secret.is_empty() {
        return Err(ApiError::BadRequest("the client secret is empty".into()));
    }

    let key_guard = state.key.read().await;
    let key = key_guard
        .as_ref()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;
    let sealed = Sealed::seal(key, name, secret.as_bytes()).map_err(ApiError::internal)?;
    let key_fingerprint = key.fingerprint();
    drop(key_guard);

    let response = state
        .cluster
        .submit(ClusterRequest::PutSecret {
            name: name.to_string(),
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
