//! `/api/configs`, `/api/secrets`, and the cluster key endpoints.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use cl_raft::state::MAX_OBJECT_BYTES;
use cl_raft::Request as ClusterRequest;
use cl_secret::{ClusterKey, Sealed};
use serde::{Deserialize, Serialize};

use crate::auth::keys::API_KEYS_CONFIG;
use crate::auth::{Identity, RequireAdmin};
use crate::cloudflare::is_reserved_cloudflare_secret;
use crate::error::ApiError;
use crate::oidc::is_reserved_oidc_secret;
use crate::redirects::REDIRECTS_CONFIG;
use crate::secrets::{self, Charset};
use crate::server::AppState;
use crate::static_routes::STATIC_ROUTES_CONFIG;
use crate::wireguard::{is_reserved_wireguard_secret, WIREGUARD_INGRESS_CONFIG};

/// Config names clouduler itself owns and never shows through the generic
/// config routes.
fn is_reserved(name: &str) -> bool {
    name == API_KEYS_CONFIG
        || name == REDIRECTS_CONFIG
        || name == STATIC_ROUTES_CONFIG
        || name == WIREGUARD_INGRESS_CONFIG
        || super::self_update::is_reserved_config(name)
}

/// Secret names clouduler itself owns and never shows through the generic
/// secret listing route.
fn is_reserved_secret(name: &str) -> bool {
    super::self_update::is_reserved_secret(name)
        || is_reserved_wireguard_secret(name)
        || is_reserved_cloudflare_secret(name)
        || is_reserved_oidc_secret(name)
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/configs", get(list_configs))
        .route("/api/configs/{name}", get(get_config).put(put_config).delete(delete_config))
        .route("/api/secrets", get(list_secrets))
        .route("/api/secrets/key", get(get_key).post(post_key))
        .route("/api/secrets/{name}", put(put_secret).delete(delete_secret))
}

#[derive(Serialize)]
struct ConfigSummary {
    name: String,
    size: usize,
    revision: u64,
    owner: Option<String>,
}

async fn list_configs(_identity: Identity, State(state): State<AppState>) -> Json<Vec<ConfigSummary>> {
    let cluster_state = state.cluster.state().await;
    Json(
        cluster_state
            .configs
            .values()
            // The api key store rides along as a config object internally,
            // but it is not a user config and must never appear here.
            .filter(|c| !is_reserved(&c.name))
            .map(|c| ConfigSummary {
                name: c.name.clone(),
                size: c.len(),
                revision: c.revision,
                owner: c.owner.clone(),
            })
            .collect(),
    )
}

#[derive(Serialize)]
struct ConfigDetail {
    name: String,
    content: String,
    revision: u64,
    owner: Option<String>,
}

async fn get_config(
    _identity: Identity,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<ConfigDetail>, ApiError> {
    if is_reserved(&name) {
        return Err(ApiError::NotFound(format!("no config {name}")));
    }
    let cluster_state = state.cluster.state().await;
    let entry = cluster_state
        .configs
        .get(&name)
        .ok_or_else(|| ApiError::NotFound(format!("no config {name}")))?;
    Ok(Json(ConfigDetail {
        name: entry.name.clone(),
        content: entry.content.clone(),
        revision: entry.revision,
        owner: entry.owner.clone(),
    }))
}

#[derive(Deserialize)]
struct PutConfigRequest {
    content: String,
}

async fn put_config(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<PutConfigRequest>,
) -> Result<StatusCode, ApiError> {
    if is_reserved(&name) {
        return Err(ApiError::BadRequest(format!("{name} is a reserved name")));
    }
    let response = state
        .cluster
        .submit(ClusterRequest::PutConfig {
            name,
            content: body.content,
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }
    Ok(StatusCode::OK)
}

async fn delete_config(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    if is_reserved(&name) {
        return Err(ApiError::BadRequest(format!("{name} is a reserved name")));
    }
    let response = state
        .cluster
        .submit(ClusterRequest::RemoveConfig { name })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct SecretSummary {
    name: String,
    revision: u64,
    owner: Option<String>,
    key_fingerprint: String,
}

/// Never includes a value, at any role: the contract is explicit that a
/// secret value leaves the sealing node exactly once, at write time, and
/// never comes back out through the api.
async fn list_secrets(_identity: Identity, State(state): State<AppState>) -> Json<Vec<SecretSummary>> {
    let cluster_state = state.cluster.state().await;
    Json(
        cluster_state
            .secrets
            .values()
            .filter(|s| !is_reserved_secret(&s.name))
            .map(|s| SecretSummary {
                name: s.name.clone(),
                revision: s.revision,
                owner: s.owner.clone(),
                key_fingerprint: s.key_fingerprint.clone(),
            })
            .collect(),
    )
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PutSecretRequest {
    Literal { value: String },
    File { content: String },
    Generate { generate: GenerateSpec },
}

#[derive(Deserialize)]
struct GenerateSpec {
    length: usize,
    charset: Charset,
}

async fn put_secret(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<PutSecretRequest>,
) -> Result<StatusCode, ApiError> {
    if is_reserved_secret(&name) {
        return Err(ApiError::BadRequest(format!("{name} is a reserved name")));
    }
    let plaintext = match body {
        PutSecretRequest::Literal { value } => value.into_bytes(),
        PutSecretRequest::File { content } => cl_raft::base64_decode(&content)
            .map_err(ApiError::BadRequest)?,
        PutSecretRequest::Generate { generate } => {
            secrets::generate(generate.length, generate.charset).map_err(|e| ApiError::BadRequest(e.to_string()))?
        }
    };
    if plaintext.len() > MAX_OBJECT_BYTES {
        return Err(ApiError::BadRequest(format!(
            "secret {name} is over the {MAX_OBJECT_BYTES} byte limit"
        )));
    }

    let key_guard = state.key.read().await;
    let key = key_guard
        .as_ref()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;
    let sealed = Sealed::seal(key, &name, &plaintext).map_err(ApiError::internal)?;
    let key_fingerprint = key.fingerprint();
    drop(key_guard);

    let response = state
        .cluster
        .submit(ClusterRequest::PutSecret {
            name,
            sealed: sealed.blob,
            key_fingerprint,
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }
    Ok(StatusCode::OK)
}

async fn delete_secret(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    if is_reserved_secret(&name) {
        return Err(ApiError::BadRequest(format!("{name} is a reserved name")));
    }
    let response = state
        .cluster
        .submit(ClusterRequest::RemoveSecret { name })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct KeyStatus {
    held: bool,
    fingerprint: Option<String>,
}

async fn get_key(_identity: Identity, State(state): State<AppState>) -> Json<KeyStatus> {
    let key = state.key.read().await;
    Json(KeyStatus {
        held: key.is_some(),
        fingerprint: key.as_ref().map(|k| k.fingerprint()),
    })
}

#[derive(Deserialize)]
struct UploadKeyRequest {
    key: String,
}

async fn post_key(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<UploadKeyRequest>,
) -> Result<StatusCode, ApiError> {
    let mut key_guard = state.key.write().await;
    if key_guard.is_some() {
        return Err(ApiError::Conflict(
            "this node already holds a cluster key; refusing to overwrite it".into(),
        ));
    }
    let key = ClusterKey::parse(&body.key).map_err(ApiError::BadRequest)?;
    key.save(&state.key_path).map_err(ApiError::internal)?;
    *key_guard = Some(key);
    Ok(StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The self-update run object rides `state.configs` like the api key
    /// store does, so it needs the same filtering out of the generic config
    /// listing, mirroring the existing `API_KEYS_CONFIG` exclusion.
    #[test]
    fn the_self_update_run_config_is_excluded_from_the_generic_listing() {
        assert!(is_reserved(cl_update::plan::RUN_CONFIG));
        assert!(!is_reserved("a_users_config"));
    }

    #[test]
    fn the_self_update_deploy_key_is_excluded_from_the_generic_secret_listing() {
        assert!(is_reserved_secret(cl_update::plan::GITHUB_TOKEN_SECRET));
        assert!(!is_reserved_secret("db_password"));
    }
}
