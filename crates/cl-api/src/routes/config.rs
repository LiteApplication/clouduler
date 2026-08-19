//! `/api/config`: the cluster config file, edited with an auto-rollback safety net.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_config::ClusterConfig;
use serde::{Deserialize, Serialize};

use crate::auth::{Identity, RequireAdmin};
use crate::config_edit::{connection_sensitive_change, PendingEdit, PendingStatus, DEFAULT_ROLLBACK_AFTER};
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/config/validate", post(validate_config))
        .route("/api/config/confirm", post(confirm))
        .route("/api/config/rollback", post(rollback))
        .route("/api/config/pending", get(pending))
}

async fn get_config(_identity: Identity, State(state): State<AppState>) -> Json<ClusterConfig> {
    Json(state.cluster_config.read().await.clone())
}

#[derive(Deserialize)]
struct PutConfigRequest {
    config: ClusterConfig,
    /// Humantime string, e.g. "60s". Defaults to 60 seconds.
    rollback_after: Option<String>,
}

async fn put_config(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<PutConfigRequest>,
) -> Result<StatusCode, ApiError> {
    body.config.validate().map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let rollback_after = match body.rollback_after {
        Some(text) => humantime::parse_duration(&text)
            .map_err(|e| ApiError::BadRequest(format!("bad rollback_after: {e}")))?,
        None => DEFAULT_ROLLBACK_AFTER,
    };

    let previous = state.cluster_config.read().await.clone();
    apply_config(&state, &body.config).await?;

    // Only arm the rollback watchdog for edits that can cut off the
    // connection this request came over. Anything else takes effect right
    // away: a mistyped failover timer doesn't need a 60 second grace period.
    if connection_sensitive_change(&previous, &body.config) {
        let mut pending = state.pending_edit.lock().await;
        *pending = Some(PendingEdit::arm(previous, state.clock(), rollback_after));
        drop(pending);

        spawn_rollback_watchdog(state, rollback_after);
    }
    Ok(StatusCode::OK)
}

/// Waits out the rollback window, then restores the previous config unless it
/// was confirmed or rolled back manually in the meantime.
fn spawn_rollback_watchdog(state: AppState, after: std::time::Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        let mut pending = state.pending_edit.lock().await;
        let Some(edit) = pending.as_mut() else { return };
        if !edit.expire(state.clock()) {
            return;
        }
        let restored = edit.previous().clone();
        drop(pending);

        match apply_config(&state, &restored).await {
            Ok(()) => {
                tracing::warn!("config edit was not confirmed in time, rolled back automatically")
            }
            Err(e) => tracing::error!(error = %e, "automatic config rollback failed"),
        }
    });
}

/// Put a config into effect: through raft first, so every node's daemon writes
/// its own copy, then locally so the node answering this request is current
/// straight away rather than after its next watcher tick.
///
/// Raft goes first on purpose. Without a quorum the edit must not land anywhere,
/// or this node comes back from a restart with a config the cluster never
/// agreed to.
pub(super) async fn apply_config(state: &AppState, config: &ClusterConfig) -> Result<(), ApiError> {
    let yaml = serde_yaml::to_string(config).map_err(ApiError::internal)?;

    let expect_generation = state
        .cluster
        .state()
        .await
        .cluster_config
        .map(|c| c.generation)
        .unwrap_or(0);

    let response = state
        .cluster
        .submit(cl_raft::Request::PutClusterConfig {
            yaml: yaml.clone(),
            expect_generation,
        })
        .await
        .map_err(|e| ApiError::Internal(format!("replicating the config: {e}")))?;
    if let Some(error) = response.error {
        return Err(ApiError::Conflict(error));
    }

    std::fs::write(&state.config_path, yaml).map_err(ApiError::internal)?;
    *state.cluster_config.write().await = config.clone();
    Ok(())
}

#[derive(Deserialize)]
struct ValidateRequest {
    config: ClusterConfig,
}

#[derive(Serialize)]
struct ValidationError {
    /// Dotted config path of the offending key, empty when the failure is not
    /// tied to one.
    path: String,
    message: String,
}

#[derive(Serialize)]
struct ValidationResponse {
    valid: bool,
    errors: Vec<ValidationError>,
}

/// Check a config without applying it.
///
/// Takes the same `{"config": ...}` envelope as `PUT /api/config` so the two
/// can be called with one body, and answers 200 with the outcome rather than a
/// 4xx: a config that fails validation is an expected answer to this question,
/// not a bad request. Validation short circuits on the first problem, so
/// `errors` holds at most one entry.
async fn validate_config(
    _identity: Identity,
    Json(body): Json<ValidateRequest>,
) -> Json<ValidationResponse> {
    match body.config.validate() {
        Ok(()) => Json(ValidationResponse {
            valid: true,
            errors: Vec::new(),
        }),
        Err(cl_config::ConfigError::Invalid { at, msg }) => Json(ValidationResponse {
            valid: false,
            errors: vec![ValidationError { path: at, message: msg }],
        }),
        Err(other) => Json(ValidationResponse {
            valid: false,
            errors: vec![ValidationError {
                path: String::new(),
                message: other.to_string(),
            }],
        }),
    }
}

async fn confirm(_admin: RequireAdmin, State(state): State<AppState>) -> Result<StatusCode, ApiError> {
    let mut pending = state.pending_edit.lock().await;
    let edit = pending
        .as_mut()
        .ok_or_else(|| ApiError::Conflict("no config edit is pending".into()))?;
    if !edit.confirm() {
        return Err(ApiError::Conflict("pending edit already settled".into()));
    }
    Ok(StatusCode::OK)
}

async fn rollback(_admin: RequireAdmin, State(state): State<AppState>) -> Result<StatusCode, ApiError> {
    let mut pending = state.pending_edit.lock().await;
    let edit = pending
        .as_mut()
        .ok_or_else(|| ApiError::Conflict("no config edit is pending".into()))?;
    if !edit.rollback_manually() {
        return Err(ApiError::Conflict("pending edit already settled".into()));
    }
    let restored = edit.previous().clone();
    drop(pending);

    apply_config(&state, &restored).await?;
    Ok(StatusCode::OK)
}

#[derive(Serialize)]
struct PendingBody {
    armed: bool,
    seconds_left: Option<u64>,
}

async fn pending(_identity: Identity, State(state): State<AppState>) -> Json<PendingBody> {
    let pending = state.pending_edit.lock().await;
    let body = match pending.as_ref() {
        Some(edit) if edit.status() == PendingStatus::Armed => PendingBody {
            armed: true,
            seconds_left: Some(edit.remaining(state.clock()).as_secs()),
        },
        _ => PendingBody {
            armed: false,
            seconds_left: None,
        },
    };
    Json(body)
}
