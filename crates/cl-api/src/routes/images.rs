//! Node-to-node image reads, for the cluster-wide docker socket
//! (`cl-dockerapi`): a node asked for an image it does not hold asks its
//! peers. HMAC authenticated like the other `/internal/` routes, never
//! reachable with a session or api key, and read-only.

use axum::body::Body;
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use cl_docker::DockerError;
use futures::TryStreamExt;
use serde::Deserialize;
use serde_json::Value;

use crate::auth::internal::InternalAuth;
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/images/internal/inspect", get(inspect))
        .route("/api/images/internal/history", get(history))
        .route("/api/images/internal/list", get(list))
        .route("/api/images/internal/save", get(save))
}

#[derive(Deserialize)]
struct NameQuery {
    name: String,
}

fn docker_error(e: DockerError) -> ApiError {
    match e {
        DockerError::NotFound(m) => ApiError::NotFound(m),
        other => ApiError::Internal(other.to_string()),
    }
}

async fn inspect(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Query(q): Query<NameQuery>,
) -> Result<Json<Value>, ApiError> {
    let docker = state.docker().await?;
    Ok(Json(
        docker
            .inspect_image_json(&q.name)
            .await
            .map_err(docker_error)?,
    ))
}

async fn history(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Query(q): Query<NameQuery>,
) -> Result<Json<Value>, ApiError> {
    let docker = state.docker().await?;
    Ok(Json(
        docker
            .image_history_json(&q.name)
            .await
            .map_err(docker_error)?,
    ))
}

async fn list(_auth: InternalAuth, State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let docker = state.docker().await?;
    Ok(Json(docker.list_images_json().await.map_err(docker_error)?))
}

/// The image as a `docker save` tar, streamed. Checked first so a missing
/// image is a 404 rather than a body that dies after the headers went out.
async fn save(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Query(q): Query<NameQuery>,
) -> Result<Response, ApiError> {
    let docker = state.docker().await?;
    docker
        .inspect_image_json(&q.name)
        .await
        .map_err(docker_error)?;
    let stream = docker.save_image(&q.name).map_err(std::io::Error::other);
    Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "application/x-tar")
        .body(Body::from_stream(stream))
        .map_err(|e| ApiError::Internal(format!("building the image response: {e}")))
}
