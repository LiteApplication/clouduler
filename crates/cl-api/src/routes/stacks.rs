//! `/api/stacks`.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_config::StackFile;
use cl_raft::Request as ClusterRequest;
use cl_secret::Sealed;
use serde::{Deserialize, Serialize};

use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/stacks", get(list))
        .route("/api/stacks/{name}", get(detail).put(deploy).delete(remove))
        .route("/api/stacks/{name}/validate", post(validate))
}

#[derive(Serialize)]
struct StackSummary {
    name: String,
    revision: u64,
}

async fn list(_identity: Identity, State(state): State<AppState>) -> Json<Vec<StackSummary>> {
    let cluster_state = state.cluster.state().await;
    Json(
        cluster_state
            .stacks
            .values()
            .map(|s| StackSummary {
                name: s.name.clone(),
                revision: s.revision,
            })
            .collect(),
    )
}

#[derive(Serialize)]
struct StackDetail {
    name: String,
    spec: String,
    revision: u64,
    configs: Vec<String>,
    secrets: Vec<String>,
}

async fn detail(
    _identity: Identity,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<StackDetail>, ApiError> {
    let cluster_state = state.cluster.state().await;
    let stack = cluster_state
        .stacks
        .get(&name)
        .ok_or_else(|| ApiError::NotFound(format!("no stack {name}")))?;
    Ok(Json(StackDetail {
        name: stack.name.clone(),
        spec: stack.spec.clone(),
        revision: stack.revision,
        configs: stack.configs.iter().cloned().collect(),
        secrets: stack.secrets.iter().cloned().collect(),
    }))
}

#[derive(Deserialize)]
struct DeployRequest {
    spec: String,
    /// Config name to base64 content, for everything the spec's `configs`
    /// section references.
    #[serde(default)]
    configs: BTreeMap<String, String>,
    /// Secret name to base64 plaintext. Sealed here before anything is
    /// written to the log.
    #[serde(default)]
    secrets: BTreeMap<String, String>,
}

async fn deploy(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<DeployRequest>,
) -> Result<StatusCode, ApiError> {
    StackFile::parse(&body.spec).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let response = state
        .cluster
        .submit(ClusterRequest::DeployStack {
            name: name.clone(),
            spec: body.spec,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }

    for (config_name, content) in &body.configs {
        let response = state
            .cluster
            .submit(ClusterRequest::PutConfig {
                name: config_name.clone(),
                content: content.clone(),
                owner: Some(name.clone()),
            })
            .await
            .map_err(ApiError::not_leader)?;
        if let Some(msg) = response.error {
            return Err(ApiError::BadRequest(msg));
        }
    }

    if !body.secrets.is_empty() {
        let key_guard = state.key.read().await;
        let key = key_guard
            .as_ref()
            .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;
        for (secret_name, content) in &body.secrets {
            let plaintext = cl_raft::base64_decode(content).map_err(ApiError::BadRequest)?;
            let sealed = Sealed::seal(key, secret_name, &plaintext).map_err(ApiError::internal)?;
            let response = state
                .cluster
                .submit(ClusterRequest::PutSecret {
                    name: secret_name.clone(),
                    sealed: sealed.blob,
                    key_fingerprint: key.fingerprint(),
                    owner: Some(name.clone()),
                })
                .await
                .map_err(ApiError::not_leader)?;
            if let Some(msg) = response.error {
                return Err(ApiError::BadRequest(msg));
            }
        }
    }

    let response = state
        .cluster
        .submit(ClusterRequest::SetStackObjects {
            name,
            configs: body.configs.keys().cloned().collect(),
            secrets: body.secrets.keys().cloned().collect(),
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }

    Ok(StatusCode::OK)
}

async fn remove(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let response = state
        .cluster
        .submit(ClusterRequest::RemoveStack { name })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ValidateRequest {
    spec: String,
}

#[derive(Serialize)]
struct ValidateResponse {
    valid: bool,
    error: Option<String>,
}

async fn validate(_identity: Identity, Json(body): Json<ValidateRequest>) -> Json<ValidateResponse> {
    match StackFile::parse(&body.spec) {
        Ok(_) => Json(ValidateResponse {
            valid: true,
            error: None,
        }),
        Err(e) => Json(ValidateResponse {
            valid: false,
            error: Some(e.to_string()),
        }),
    }
}
