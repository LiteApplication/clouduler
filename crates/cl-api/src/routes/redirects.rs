//! `/api/redirects`: CRUD over UI-managed proxy redirects. See
//! `crate::redirects` for why these live outside `cluster.yaml`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use cl_raft::Request as ClusterRequest;
use serde::Deserialize;

use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::redirects::{new_id, RedirectRule, RedirectStore, REDIRECTS_CONFIG};
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/redirects", get(list).post(create))
        .route("/api/redirects/{id}", axum::routing::put(update).delete(remove))
}

async fn list(_identity: Identity, State(state): State<AppState>) -> Json<Vec<RedirectRule>> {
    let cluster_state = state.cluster.state().await;
    Json(RedirectStore::load(&cluster_state).rules)
}

#[derive(Deserialize)]
struct RedirectBody {
    host: String,
    network: String,
    #[serde(default)]
    path_prefix: Option<String>,
    target: String,
    #[serde(default)]
    preserve_path: bool,
    status: u16,
}

async fn create(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<RedirectBody>,
) -> Result<Json<RedirectRule>, ApiError> {
    validate(&state, &body).await?;

    let cluster_state = state.cluster.state().await;
    let mut store = RedirectStore::load(&cluster_state);
    let rule = RedirectRule {
        id: new_id(),
        host: body.host,
        network: body.network,
        path_prefix: body.path_prefix,
        target: body.target,
        preserve_path: body.preserve_path,
        status: body.status,
    };
    store.rules.push(rule.clone());
    submit_store(&state, &store).await?;
    Ok(Json(rule))
}

async fn update(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RedirectBody>,
) -> Result<Json<RedirectRule>, ApiError> {
    validate(&state, &body).await?;

    let cluster_state = state.cluster.state().await;
    let mut store = RedirectStore::load(&cluster_state);
    let Some(existing) = store.rules.iter_mut().find(|r| r.id == id) else {
        return Err(ApiError::NotFound(format!("no redirect {id}")));
    };
    existing.host = body.host;
    existing.network = body.network;
    existing.path_prefix = body.path_prefix;
    existing.target = body.target;
    existing.preserve_path = body.preserve_path;
    existing.status = body.status;
    let updated = existing.clone();

    submit_store(&state, &store).await?;
    Ok(Json(updated))
}

async fn remove(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let cluster_state = state.cluster.state().await;
    let mut store = RedirectStore::load(&cluster_state);
    let before = store.rules.len();
    store.rules.retain(|r| r.id != id);
    if store.rules.len() == before {
        return Err(ApiError::NotFound(format!("no redirect {id}")));
    }
    submit_store(&state, &store).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn validate(state: &AppState, body: &RedirectBody) -> Result<(), ApiError> {
    if body.host.trim().is_empty() {
        return Err(ApiError::BadRequest("host must not be empty".into()));
    }
    if body.target.trim().is_empty() {
        return Err(ApiError::BadRequest("target must not be empty".into()));
    }
    if !RedirectRule::valid_status(body.status) {
        return Err(ApiError::BadRequest(format!(
            "status must be 301, 302, 307 or 308, got {}",
            body.status
        )));
    }
    let cluster_config = state.cluster_config.read().await;
    if !cluster_config.networks.contains_key(&body.network) {
        return Err(ApiError::BadRequest(format!("no network named {}", body.network)));
    }
    Ok(())
}

async fn submit_store(state: &AppState, store: &RedirectStore) -> Result<(), ApiError> {
    let response = state
        .cluster
        .submit(ClusterRequest::PutConfig {
            name: REDIRECTS_CONFIG.to_string(),
            content: store.encode(),
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Internal(msg));
    }
    Ok(())
}
