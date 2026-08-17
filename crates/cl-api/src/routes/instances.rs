//! `/api/instances`.

use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_docker::{LogOptions, Tail};
use cl_raft::ids::InstanceId;
use cl_raft::state::DesiredState;
use cl_raft::Request as ClusterRequest;
use serde::{Deserialize, Serialize};

use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::server::AppState;
use crate::stream;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/instances", get(list))
        .route("/api/instances/{id}", get(detail).patch(patch_instance))
        .route("/api/instances/{id}/restart", post(restart))
        .route("/api/instances/{id}/move", post(move_instance))
        .route("/api/instances/{id}/update", post(update))
        .route("/api/instances/{id}/logs", get(logs))
        .route("/api/instances/{id}/stats", get(stats))
}

#[derive(Serialize)]
struct InstanceSummary {
    id: String,
    node: String,
    desired: String,
    running: Option<bool>,
}

async fn list(_identity: Identity, State(state): State<AppState>) -> Json<Vec<InstanceSummary>> {
    let cluster_state = state.cluster.state().await;
    let mut out = Vec::with_capacity(cluster_state.assignments.len());
    for assignment in cluster_state.assignments.values() {
        let running = if assignment.node == state.node_name {
            state
                .docker
                .inspect(&assignment.instance.container_name())
                .await
                .ok()
                .flatten()
                .map(|c| c.running)
        } else {
            None
        };
        out.push(InstanceSummary {
            id: assignment.instance.to_string(),
            node: assignment.node.clone(),
            desired: format!("{:?}", assignment.desired).to_lowercase(),
            running,
        });
    }
    Json(out)
}

fn parse_id(raw: &str) -> Result<InstanceId, ApiError> {
    raw.parse().map_err(ApiError::BadRequest)
}

async fn detail(
    _identity: Identity,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<InstanceSummary>, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.state().await;
    let assignment = cluster_state
        .assignments
        .get(&instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;

    let running = if assignment.node == state.node_name {
        state
            .docker
            .inspect(&instance.container_name())
            .await
            .ok()
            .flatten()
            .map(|c| c.running)
    } else {
        None
    };

    Ok(Json(InstanceSummary {
        id: instance.to_string(),
        node: assignment.node.clone(),
        desired: format!("{:?}", assignment.desired).to_lowercase(),
        running,
    }))
}

async fn restart(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.state().await;
    let assignment = cluster_state
        .assignments
        .get(&instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;
    if assignment.node != state.node_name {
        return Err(ApiError::Conflict(format!(
            "instance {id} runs on {}, not this node",
            assignment.node
        )));
    }
    state
        .docker
        .restart(&instance.container_name(), Duration::from_secs(10))
        .await
        .map_err(ApiError::internal)?;
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
struct MoveRequest {
    node: String,
}

async fn move_instance(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<MoveRequest>,
) -> Result<StatusCode, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.state().await;
    let drives = required_drives(&cluster_state, &instance);

    let response = state
        .cluster
        .submit(ClusterRequest::BeginMigration {
            instance,
            to: body.node,
            drives,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::OK)
}

/// The scheduler stamps a volume's drive onto assignments indirectly; cl-api
/// only sees the assignment, not the service spec that named the drives it
/// needs. Best it can do without re-parsing the stack spec is pass an empty
/// set, which `check_placement` treats as "no drive requirements".
fn required_drives(_state: &cl_raft::ClusterState, _instance: &InstanceId) -> std::collections::BTreeSet<String> {
    std::collections::BTreeSet::new()
}

#[derive(Deserialize)]
struct PatchInstanceRequest {
    desired: DesiredState,
}

async fn patch_instance(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchInstanceRequest>,
) -> Result<StatusCode, ApiError> {
    let instance = parse_id(&id)?;
    let response = state
        .cluster
        .submit(ClusterRequest::SetDesiredState {
            instance,
            desired: body.desired,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::OK)
}

async fn update(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.state().await;
    let assignment = cluster_state
        .assignments
        .get(&instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;
    if assignment.node != state.node_name {
        return Err(ApiError::Conflict(format!(
            "instance {id} runs on {}, not this node",
            assignment.node
        )));
    }
    let name = instance.container_name();
    let current = state
        .docker
        .inspect(&name)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::NotFound(format!("container {name} does not exist on this node")))?;

    let digest = state.docker.pull(&current.image).await.map_err(ApiError::internal)?;
    state
        .cluster
        .submit(ClusterRequest::SetImageDigest { instance, digest })
        .await
        .map_err(ApiError::not_leader)?;
    state
        .docker
        .restart(&name, Duration::from_secs(10))
        .await
        .map_err(ApiError::internal)?;
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
struct LogsQuery {
    #[serde(default)]
    follow: bool,
    tail: Option<u32>,
    since: Option<i64>,
}

async fn logs(
    _identity: Identity,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.state().await;
    let assignment = cluster_state
        .assignments
        .get(&instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;
    if assignment.node != state.node_name {
        return Err(ApiError::Conflict(format!(
            "instance {id} runs on {}, not this node; connect there for its logs",
            assignment.node
        )));
    }

    let opts = LogOptions {
        follow: query.follow,
        tail: query.tail.map(Tail::Lines).unwrap_or(Tail::Lines(200)),
        since: query.since,
    };
    let container_name = instance.container_name();
    let docker = state.docker.clone();
    let log_stream = async_stream::stream! {
        let stream = docker.logs(&container_name, opts);
        futures::pin_mut!(stream);
        while let Some(item) = futures::StreamExt::next(&mut stream).await {
            yield item;
        }
    };
    Ok(stream::container_log_events(log_stream))
}

async fn stats(
    _identity: Identity,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.state().await;
    let assignment = cluster_state
        .assignments
        .get(&instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;
    if assignment.node != state.node_name {
        return Err(ApiError::Conflict(format!(
            "instance {id} runs on {}, not this node; connect there for its stats",
            assignment.node
        )));
    }

    let samples = crate::docker_stats::stream(instance.container_name()).await?;
    Ok(stream::container_stat_events(samples))
}
