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
    /// What the cluster wants: "running" or "stopped".
    desired: String,
    /// What the container is actually doing. `unknown` when the instance runs
    /// on another node, since there is no rpc path to inspect a container
    /// elsewhere; that is a real gap, not an error, so it gets its own value
    /// rather than being reported as stopped.
    state: String,
    running: Option<bool>,
    health: String,
    image: Option<String>,
    image_digest: Option<String>,
    started_at: Option<String>,
    migration: Option<MigrationSummary>,
}

#[derive(Serialize)]
struct MigrationSummary {
    to: String,
    phase: String,
}

/// The state names the ui and the test harness both match on. Kept as strings
/// rather than a bare bool because "we cannot see it from here" is a distinct
/// answer from "it is not running".
const STATE_RUNNING: &str = "running";
const STATE_STOPPED: &str = "stopped";
const STATE_UNKNOWN: &str = "unknown";

async fn summarize(state: &AppState, assignment: &cl_raft::Assignment) -> InstanceSummary {
    let local = assignment.node == state.node_name;
    let container = if local {
        state
            .docker
            .inspect(&assignment.instance.container_name())
            .await
            .ok()
            .flatten()
    } else {
        None
    };

    let running = container.as_ref().map(|c| c.running);
    let instance_state = match (local, running) {
        (true, Some(true)) => STATE_RUNNING,
        (true, Some(false)) => STATE_STOPPED,
        // Assigned here but no container yet: it is on its way.
        (true, None) => STATE_STOPPED,
        (false, _) => STATE_UNKNOWN,
    };

    InstanceSummary {
        id: assignment.instance.to_string(),
        node: assignment.node.clone(),
        desired: format!("{:?}", assignment.desired).to_lowercase(),
        state: instance_state.to_string(),
        running,
        health: container
            .as_ref()
            .and_then(|c| c.health.as_ref())
            .map(|h| format!("{h:?}").to_lowercase())
            .unwrap_or_else(|| "none".to_string()),
        image: container.as_ref().map(|c| c.image.clone()),
        image_digest: assignment.image_digest.clone(),
        started_at: container.as_ref().and_then(|c| c.started_at.clone()),
        migration: assignment.migration.as_ref().map(|m| MigrationSummary {
            to: m.to.clone(),
            phase: format!("{:?}", m.phase).to_lowercase(),
        }),
    }
}

async fn list(_identity: Identity, State(state): State<AppState>) -> Json<Vec<InstanceSummary>> {
    let cluster_state = state.cluster.state().await;
    let mut out = Vec::with_capacity(cluster_state.assignments.len());
    for assignment in cluster_state.assignments.values() {
        out.push(summarize(&state, assignment).await);
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

    Ok(Json(summarize(&state, assignment).await))
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
