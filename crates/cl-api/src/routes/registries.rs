//! `/api/registries`: the container registry accounts the Updates view uses.
//!
//! A login is sealed under a reserved cluster secret name, replicated through raft and never readable back
//! (`list` returns the username only). See `cl_sched::registries` for the format and pull selection; verification
//! and server-name normalization live there too, shared with the `clouduler registry` cli.
//!
//! Every login is checked before it is stored: a wrong password accepted silently would fail later, on another
//! node, mid-pull.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_raft::Request as ClusterRequest;
use cl_sched::registries::{normalize_server, secret_name, verify, Registries, StoredLogin};
use serde::{Deserialize, Serialize};

use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/registries", get(list).put(put_registry))
        .route("/api/registries/verify", post(verify_only))
        .route(
            "/api/registries/{server}",
            axum::routing::delete(delete_registry),
        )
}

/// A node with no cluster key yet gets an empty set and pulls anonymously, as before logins existed, rather than
/// refusing to pull while the key is delivered.
pub(crate) async fn load(state: &AppState) -> Registries {
    let Some(key) = state.key.read().await.clone() else {
        return Registries::default();
    };
    Registries::load(&state.cluster.state().await, &key)
}

#[derive(Serialize)]
struct RegistryAccount {
    server: String,
    username: String,
}

async fn list(
    _identity: Identity,
    State(state): State<AppState>,
) -> Result<Json<Vec<RegistryAccount>>, ApiError> {
    let cluster_state = state.cluster.snapshot().await;
    let key = state
        .key
        .read()
        .await
        .clone()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;

    let registries = Registries::load(&cluster_state, &key);
    let mut accounts: Vec<RegistryAccount> = registries
        .accounts()
        .map(|(server, username)| RegistryAccount {
            server: server.to_string(),
            username: username.to_string(),
        })
        .collect();
    accounts.sort_by(|a, b| a.server.cmp(&b.server));
    Ok(Json(accounts))
}

#[derive(Deserialize)]
struct LoginRequest {
    server: String,
    username: String,
    password: String,
}

async fn verify_only(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<StatusCode, ApiError> {
    let (server, username, password) = validated(body)?;
    verify(&state.http, &server, &username, &password)
        .await
        .map_err(ApiError::BadRequest)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Rotation is a plain overwrite: one copy, read by every node out of raft.
async fn put_registry(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<StatusCode, ApiError> {
    let (server, username, password) = validated(body)?;
    verify(&state.http, &server, &username, &password)
        .await
        .map_err(ApiError::BadRequest)?;

    let key = state
        .key
        .read()
        .await
        .clone()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;

    let login = StoredLogin {
        server,
        username,
        password,
    };
    let (name, sealed) = login.seal(&key).map_err(ApiError::internal)?;

    let response = state
        .cluster
        .submit(ClusterRequest::PutSecret {
            name,
            sealed: sealed.blob,
            key_fingerprint: key.fingerprint(),
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_registry(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(server): Path<String>,
) -> Result<StatusCode, ApiError> {
    let response = state
        .cluster
        .submit(ClusterRequest::RemoveSecret {
            name: secret_name(&normalize_server(&server)),
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::NO_CONTENT)
}

fn validated(body: LoginRequest) -> Result<(String, String, String), ApiError> {
    let server = normalize_server(&body.server);
    if server.is_empty() {
        return Err(ApiError::BadRequest("the registry host is empty".into()));
    }
    let username = body.username.trim().to_string();
    if username.is_empty() {
        return Err(ApiError::BadRequest("the username is empty".into()));
    }
    // Not trimmed: whitespace can be part of a password, and changing it yields a login that verifies here and fails elsewhere.
    if body.password.is_empty() {
        return Err(ApiError::BadRequest("the password is empty".into()));
    }
    Ok((server, username, body.password))
}
