//! `/api/stacks`.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_config::{StackFile, StackRequirements};
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
    used_configs: Vec<String>,
    used_secrets: Vec<String>,
    requirements: StackRequirements,
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
    // The spec was already validated at deploy time, so a parse failure here
    // would mean corrupted state, not a bad request.
    let requirements = match StackFile::parse(&stack.spec) {
        Ok(parsed) => {
            let mut r = parsed.requirements();
            let cfg = state.cluster_config.read().await;
            r.hosts = parsed.hosts(&cfg);
            r
        }
        Err(_) => StackRequirements::default(),
    };
    Ok(Json(StackDetail {
        name: stack.name.clone(),
        spec: stack.spec.clone(),
        revision: stack.revision,
        configs: stack.configs.iter().cloned().collect(),
        secrets: stack.secrets.iter().cloned().collect(),
        used_configs: stack.used_configs.iter().cloned().collect(),
        used_secrets: stack.used_secrets.iter().cloned().collect(),
        requirements,
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
    let secrets_to_write = {
        let parsed =
            StackFile::parse(&body.spec).map_err(|e| ApiError::BadRequest(e.to_string()))?;
        let requirements = parsed.requirements();
        let cluster_state = state.cluster.state().await;
        for config_name in &requirements.file_configs {
            if !body.configs.contains_key(config_name)
                && !cluster_state.configs.contains_key(config_name)
            {
                return Err(ApiError::BadRequest(format!(
                    "config {config_name} needs content: it has no existing value to keep"
                )));
            }
        }
        for secret_name in &requirements.file_secrets {
            if !body.secrets.contains_key(secret_name)
                && !cluster_state.secrets.contains_key(secret_name)
            {
                return Err(ApiError::BadRequest(format!(
                    "secret {secret_name} needs content: it has no existing value to keep"
                )));
            }
        }
        body.secrets
            .into_iter()
            .filter(|(k, _)| requirements.file_secrets.contains(k))
            .collect::<BTreeMap<_, _>>()
    };

    if !secrets_to_write.is_empty() {
        let key_guard = state.key.read().await;
        let key = key_guard
            .as_ref()
            .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;
        for (secret_name, content) in &secrets_to_write {
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

    let configs: BTreeMap<String, Vec<u8>> = body
        .configs
        .into_iter()
        .map(|(name, content)| {
            Ok((
                name,
                cl_raft::base64_decode(&content).map_err(ApiError::BadRequest)?,
            ))
        })
        .collect::<Result<_, ApiError>>()?;
    deploy_from_files(&state, &name, body.spec, configs).await?;
    Ok(StatusCode::OK)
}

/// The reusable core of `deploy`: parses+validates the spec, submits it, then
/// writes every provided config it owns and records what it owns. Shared with
/// `routes::git_backup`'s reverse (git -> cluster) sync, which never has
/// secret content to supply (git-mirrored secrets are age-encrypted for the
/// admin, not this cluster's key, so they can't be written back this way) and
/// so only ever calls this, never the secrets branch above.
pub(crate) async fn deploy_from_files(
    state: &AppState,
    name: &str,
    spec: String,
    configs: BTreeMap<String, Vec<u8>>,
) -> Result<(), ApiError> {
    let parsed = StackFile::parse(&spec).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let requirements = parsed.requirements();

    // The names this stack owns come from its own spec, not from whatever the
    // caller happened to include in this request: a redeploy that doesn't
    // resend an unchanged config's content must not drop the stack's claim on
    // it, and a config renamed out of the spec must not linger as owned.
    let cluster_state = state.cluster.state().await;
    for config_name in &requirements.file_configs {
        if !configs.contains_key(config_name) && !cluster_state.configs.contains_key(config_name) {
            return Err(ApiError::BadRequest(format!(
                "config {config_name} needs content: it has no existing value to keep"
            )));
        }
    }

    let response = state
        .cluster
        .submit(ClusterRequest::DeployStack {
            name: name.to_string(),
            spec,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }

    for config_name in &requirements.file_configs {
        let Some(content) = configs.get(config_name) else {
            continue;
        };
        let response = state
            .cluster
            .submit(ClusterRequest::PutConfig {
                name: config_name.clone(),
                content: cl_backup::tree::encode_config_content(content),
                owner: Some(name.to_string()),
            })
            .await
            .map_err(ApiError::not_leader)?;
        if let Some(msg) = response.error {
            return Err(ApiError::BadRequest(msg));
        }
    }

    let response = state
        .cluster
        .submit(ClusterRequest::SetStackObjects {
            name: name.to_string(),
            configs: requirements.file_configs,
            secrets: requirements.file_secrets,
            used_configs: requirements.configs,
            used_secrets: requirements.secrets,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }

    Ok(())
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
struct ValidationError {
    path: String,
    message: String,
}

#[derive(Serialize)]
struct ValidateResponse {
    valid: bool,
    errors: Vec<ValidationError>,
    requirements: Option<StackRequirements>,
    /// The same breakdown as `requirements`, one entry per service: what the
    /// ui shows as a separate placement table per container instead of one
    /// table that can't say which constraint belongs to which service.
    per_service: BTreeMap<String, StackRequirements>,
}

async fn validate(
    _identity: Identity,
    State(state): State<AppState>,
    Json(body): Json<ValidateRequest>,
) -> Json<ValidateResponse> {
    match StackFile::parse(&body.spec) {
        Ok(parsed) => {
            let mut requirements = parsed.requirements();
            let cfg = state.cluster_config.read().await;
            requirements.hosts = parsed.hosts(&cfg);
            Json(ValidateResponse {
                valid: true,
                errors: Vec::new(),
                requirements: Some(requirements),
                per_service: parsed.service_requirements(),
            })
        }
        Err(e) => Json(ValidateResponse {
            valid: false,
            errors: vec![ValidationError {
                path: String::new(),
                message: e.to_string(),
            }],
            requirements: None,
            per_service: BTreeMap::new(),
        }),
    }
}
