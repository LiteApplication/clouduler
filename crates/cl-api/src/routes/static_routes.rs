//! `/api/static-routes`: CRUD over UI-managed static/external proxy backends.
//! See `crate::static_routes` for why these live outside `cluster.yaml`.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use cl_config::{ByteSize, Middleware};
use cl_raft::Request as ClusterRequest;
use serde::Deserialize;

use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::static_routes::{new_id, StaticRouteRule, StaticRouteStore, STATIC_ROUTES_CONFIG};
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/static-routes", get(list).post(create))
        .route("/api/static-routes/{id}", axum::routing::put(update).delete(remove))
}

async fn list(_identity: Identity, State(state): State<AppState>) -> Json<Vec<StaticRouteRule>> {
    let cluster_state = state.cluster.state().await;
    Json(StaticRouteStore::load(&cluster_state).routes)
}

#[derive(Deserialize)]
struct StaticRouteBody {
    host: String,
    network: String,
    backend: String,
    #[serde(default)]
    path_prefix: Option<String>,
    #[serde(default = "default_true")]
    tls: bool,
    #[serde(default)]
    middlewares: Vec<Middleware>,
    #[serde(default)]
    max_body_size: Option<ByteSize>,
    #[serde(default = "default_true")]
    websockets: bool,
    #[serde(default)]
    preserve_host: bool,
    #[serde(default, with = "humantime_serde::option")]
    response_timeout: Option<Duration>,
}

fn default_true() -> bool {
    true
}

async fn create(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<StaticRouteBody>,
) -> Result<Json<StaticRouteRule>, ApiError> {
    validate(&state, &body).await?;

    let cluster_state = state.cluster.state().await;
    let mut store = StaticRouteStore::load(&cluster_state);
    let route = StaticRouteRule {
        id: new_id(),
        host: body.host,
        network: body.network,
        backend: body.backend,
        path_prefix: body.path_prefix,
        tls: body.tls,
        middlewares: body.middlewares,
        max_body_size: body.max_body_size,
        websockets: body.websockets,
        preserve_host: body.preserve_host,
        response_timeout: body.response_timeout,
    };
    store.routes.push(route.clone());
    submit_store(&state, &store).await?;
    Ok(Json(route))
}

async fn update(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<StaticRouteBody>,
) -> Result<Json<StaticRouteRule>, ApiError> {
    validate(&state, &body).await?;

    let cluster_state = state.cluster.state().await;
    let mut store = StaticRouteStore::load(&cluster_state);
    let Some(existing) = store.routes.iter_mut().find(|r| r.id == id) else {
        return Err(ApiError::NotFound(format!("no static route {id}")));
    };
    existing.host = body.host;
    existing.network = body.network;
    existing.backend = body.backend;
    existing.path_prefix = body.path_prefix;
    existing.tls = body.tls;
    existing.middlewares = body.middlewares;
    existing.max_body_size = body.max_body_size;
    existing.websockets = body.websockets;
    existing.preserve_host = body.preserve_host;
    existing.response_timeout = body.response_timeout;
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
    let mut store = StaticRouteStore::load(&cluster_state);
    let before = store.routes.len();
    store.routes.retain(|r| r.id != id);
    if store.routes.len() == before {
        return Err(ApiError::NotFound(format!("no static route {id}")));
    }
    submit_store(&state, &store).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn validate(state: &AppState, body: &StaticRouteBody) -> Result<(), ApiError> {
    if body.host.trim().is_empty() {
        return Err(ApiError::BadRequest("host must not be empty".into()));
    }
    let (host, port) = body
        .backend
        .rsplit_once(':')
        .ok_or_else(|| ApiError::BadRequest("backend must be host:port".into()))?;
    if host.is_empty() {
        return Err(ApiError::BadRequest("backend must be host:port".into()));
    }
    if port.parse::<u16>().is_err() {
        return Err(ApiError::BadRequest(format!("{port} is not a valid port")));
    }
    let cluster_config = state.cluster_config.read().await;
    if !cluster_config.networks.contains_key(&body.network) {
        return Err(ApiError::BadRequest(format!("no network named {}", body.network)));
    }
    Ok(())
}

async fn submit_store(state: &AppState, store: &StaticRouteStore) -> Result<(), ApiError> {
    let response = state
        .cluster
        .submit(ClusterRequest::PutConfig {
            name: STATIC_ROUTES_CONFIG.to_string(),
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
