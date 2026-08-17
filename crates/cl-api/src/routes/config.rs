//! `/api/config`: the cluster config file, edited with an auto-rollback safety net.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_config::ClusterConfig;
use serde::{Deserialize, Serialize};

use crate::auth::{Identity, RequireAdmin};
use crate::config_edit::{PendingEdit, PendingStatus, DEFAULT_ROLLBACK_AFTER};
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
    write_config_file(&state, &body.config)?;
    *state.cluster_config.write().await = body.config;

    let mut pending = state.pending_edit.lock().await;
    *pending = Some(PendingEdit::arm(previous, state.clock(), rollback_after));
    drop(pending);

    spawn_rollback_watchdog(state, rollback_after);
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

        if write_config_file(&state, &restored).is_ok() {
            *state.cluster_config.write().await = restored;
            tracing::warn!("config edit was not confirmed in time, rolled back automatically");
        } else {
            tracing::error!("automatic config rollback could not write the config file");
        }
    });
}

fn write_config_file(state: &AppState, config: &ClusterConfig) -> Result<(), ApiError> {
    let yaml = serde_yaml::to_string(config).map_err(ApiError::internal)?;
    std::fs::write(&state.config_path, yaml).map_err(ApiError::internal)
}

async fn validate_config(
    _identity: Identity,
    Json(config): Json<ClusterConfig>,
) -> Result<StatusCode, ApiError> {
    config.validate().map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(StatusCode::OK)
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

    write_config_file(&state, &restored)?;
    *state.cluster_config.write().await = restored;
    Ok(StatusCode::OK)
}

#[derive(Serialize)]
struct PendingBody {
    armed: bool,
    remaining_seconds: Option<u64>,
}

async fn pending(_identity: Identity, State(state): State<AppState>) -> Json<PendingBody> {
    let pending = state.pending_edit.lock().await;
    let body = match pending.as_ref() {
        Some(edit) if edit.status() == PendingStatus::Armed => PendingBody {
            armed: true,
            remaining_seconds: Some(edit.remaining(state.clock()).as_secs()),
        },
        _ => PendingBody {
            armed: false,
            remaining_seconds: None,
        },
    };
    Json(body)
}
