//! `/api/oidc/client-secret`: setting the OIDC client secret from the web ui.
//!
//! The secret is write-only over the api. It is sealed and stored as a
//! cluster secret (`cl_api::oidc::OIDC_CLIENT_SECRET`), the same pattern the
//! Cloudflare token route uses (`crate::routes::cloudflare`); only its
//! presence is ever reported back, there is no route that returns the value,
//! so a stolen session cannot read out a secret someone else installed.
//!
//! Replicated through raft rather than written to a file on one node, so
//! every node can open it with the cluster key and login keeps working after
//! a failover to a different leader.

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
    /// True once a secret is stored, which is what the ui keys the "you
    /// cannot see or change this here" state off.
    configured: bool,
}

async fn secret_status(_identity: Identity, State(state): State<AppState>) -> Json<SecretStatus> {
    status_of(&state, OIDC_CLIENT_SECRET).await
}

/// Same for the reverse proxy login portal's client (`proxy.auth`).
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

/// Seals and stores the secret, replacing whatever was there before:
/// rotation is safe now that there is exactly one copy of it, replicated to
/// every node, rather than a file an operator would otherwise have to update
/// by hand on each manager.
///
/// Settable whether or not `oidc` is currently enabled in the config, so a
/// secret can be staged ahead of flipping OIDC on rather than forcing that
/// ordering on the operator.
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
