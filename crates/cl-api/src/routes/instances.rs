//! `/api/instances`.

use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_config::Service;
use cl_docker::{LogOptions, Tail};
use cl_raft::ids::InstanceId;
use cl_raft::state::DesiredState;
use cl_raft::Request as ClusterRequest;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::auth::internal::InternalAuth;
use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::internal_peer;
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
        // Node-to-node only, HMAC authenticated: what the public routes above
        // forward to when the instance they were asked about lives on another
        // node. See `internal_peer`.
        .route("/api/instances/internal/{id}/summary", get(internal_summary))
        .route("/api/instances/internal/{id}/logs", get(internal_logs))
        .route("/api/instances/internal/{id}/stats", get(internal_stats))
        .route("/api/instances/internal/{id}/restart", post(internal_restart))
        .route("/api/instances/internal/{id}/update", post(internal_update))
}

#[derive(Serialize, Deserialize)]
struct InstanceSummary {
    id: String,
    node: String,
    /// What the cluster wants: "running" or "stopped".
    desired: String,
    /// What the container is actually doing.
    state: String,
    running: Option<bool>,
    health: String,
    image: Option<String>,
    image_digest: Option<String>,
    started_at: Option<String>,
    migration: Option<MigrationSummary>,
    /// Why this instance has no assignment at all, set only when `state` is
    /// `"pending_placement"`: no node in the cluster currently satisfies
    /// what its service needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    placement_error: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct MigrationSummary {
    to: String,
    phase: String,
}

/// The state names the ui and the test harness both match on. Kept as strings
/// rather than a bare enum because "we cannot see it from here" is a distinct
/// answer from "it is not running", and because these cross the wire as-is.
const STATE_RUNNING: &str = "running";
const STATE_STOPPED: &str = "stopped";
const STATE_STOPPING: &str = "stopping";
const STATE_RESTARTING: &str = "restarting";
const STATE_EXITED: &str = "exited";
const STATE_PULLING: &str = "pulling";
const STATE_PENDING_PLACEMENT: &str = "pending_placement";
const STATE_UNKNOWN: &str = "unknown";

fn internal_summary_path(instance: &InstanceId) -> String {
    format!("/api/instances/internal/{instance}/summary")
}

/// Answers for any assignment, wherever it runs: local ones straight from
/// docker, remote ones by asking the node that owns it over the internal api
/// (see `internal_peer`). A remote node that cannot be reached in time falls
/// back to `summarize_local`'s unknown/none answer rather than failing the
/// whole list — the same degrade a caller already saw before cross-node
/// forwarding existed.
async fn summarize(state: &AppState, assignment: &cl_raft::Assignment) -> InstanceSummary {
    if assignment.node == state.node_name {
        return summarize_local(state, assignment).await;
    }
    match internal_peer::fetch_json::<InstanceSummary>(
        state,
        &assignment.node,
        &internal_summary_path(&assignment.instance),
    )
    .await
    {
        Ok(summary) => summary,
        Err(e) => {
            warn!(node = %assignment.node, instance = %assignment.instance, error = %e, "could not fetch instance summary from its node");
            summarize_local(state, assignment).await
        }
    }
}

/// The actual docker inspection, valid only on the node running the
/// container. Also the graceful "unknown" answer for a remote assignment,
/// used both as `summarize`'s fallback and as the direct answer for an
/// instance genuinely assigned elsewhere (docker down, container not up yet).
async fn summarize_local(state: &AppState, assignment: &cl_raft::Assignment) -> InstanceSummary {
    let local = assignment.node == state.node_name;
    // A local instance whose docker is unreachable reads as unknown, the same
    // as one on another node: in both cases nobody here can say what the
    // container is doing, which is a different answer from "stopped".
    let container = match (local, state.docker.read().await.clone()) {
        (true, Some(docker)) => docker
            .inspect(&assignment.instance.container_name())
            .await
            .ok()
            .flatten(),
        _ => None,
    };

    let running = container.as_ref().map(|c| c.running);
    let instance_state = if !local {
        STATE_UNKNOWN
    } else {
        match &container {
            Some(c) => run_state_to_summary(c.status, assignment.desired),
            // Assigned here but no container yet: either the image is still
            // being fetched, or it is about to be created.
            None if state.pulling.read().await.contains(&assignment.instance) => STATE_PULLING,
            None => STATE_STOPPED,
        }
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
        placement_error: None,
    }
}

/// Translate docker's own lifecycle status into the state string the ui
/// shows, folding in what the cluster actually asked for: the same
/// `RunState::Exited` means "it crashed" when we wanted it running, and just
/// "stopped" when we asked it to stop.
fn run_state_to_summary(status: cl_docker::RunState, desired: DesiredState) -> &'static str {
    use cl_docker::RunState;
    match status {
        RunState::Running => STATE_RUNNING,
        RunState::Restarting => STATE_RESTARTING,
        RunState::Stopping | RunState::Removing => STATE_STOPPING,
        RunState::Exited | RunState::Dead => {
            if desired == DesiredState::Running {
                STATE_EXITED
            } else {
                STATE_STOPPED
            }
        }
        RunState::Created | RunState::Paused | RunState::Unknown => STATE_STOPPED,
    }
}

async fn list(_identity: Identity, State(state): State<AppState>) -> Json<Vec<InstanceSummary>> {
    let cluster_state = state.cluster.state().await;
    // Concurrent rather than one at a time: a single unreachable node whose
    // instances sit behind `FETCH_TIMEOUT` must not serialize onto the whole
    // list's latency.
    let mut out = futures::future::join_all(cluster_state.assignments.values().map(|a| summarize(&state, a))).await;
    out.extend(pending_placements(&state, &cluster_state).await);
    Json(out)
}

/// Instances every deployed stack wants but that have no assignment at all,
/// because no node in the cluster currently satisfies what they need. Nothing
/// in `cluster_state.assignments` represents these, so they are computed live
/// from the same placement logic the scheduler runs, rather than stored.
async fn pending_placements(state: &AppState, cluster_state: &cl_raft::ClusterState) -> Vec<InstanceSummary> {
    let config = state.cluster_config.read().await;
    let colocate_proxy_hosts = config.proxy.dns_publish == cl_config::DnsPublishMode::OwningNode;
    let desired = cl_sched::desired_instances(
        cluster_state,
        &config.egress_networks(),
        &config.port_gated_networks(),
        colocate_proxy_hosts,
    );
    let served_drives = config.served_drives();
    drop(config);

    cl_sched::place(cluster_state, &desired, &served_drives)
        .unplaceable
        .into_iter()
        .map(|u| InstanceSummary {
            id: u.instance.to_string(),
            node: String::new(),
            desired: "running".to_string(),
            state: STATE_PENDING_PLACEMENT.to_string(),
            running: None,
            health: "none".to_string(),
            image: None,
            image_digest: None,
            started_at: None,
            migration: None,
            placement_error: Some(u.reason),
        })
        .collect()
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

/// Node-to-node: always answers about this node's own docker, regardless of
/// what the replicated assignment says, since the caller already checked
/// that before forwarding here.
async fn internal_summary(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<InstanceSummary>, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.state().await;
    let assignment = cluster_state
        .assignments
        .get(&instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;
    Ok(Json(summarize_local(&state, assignment).await))
}

fn internal_restart_path(instance: &InstanceId) -> String {
    format!("/api/instances/internal/{instance}/restart")
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
        let node = assignment.node.clone();
        internal_peer::post_empty(&state, &node, &internal_restart_path(&instance)).await?;
        return Ok(StatusCode::OK);
    }
    restart_local(&state, &instance).await?;
    Ok(StatusCode::OK)
}

async fn restart_local(state: &AppState, instance: &InstanceId) -> Result<(), ApiError> {
    state
        .docker()
        .await?
        .restart(&instance.container_name(), Duration::from_secs(10))
        .await
        .map_err(ApiError::internal)
}

/// Node-to-node: restarts this node's own container. The caller already
/// checked the assignment and the caller's own admin auth before forwarding
/// here.
async fn internal_restart(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let instance = parse_id(&id)?;
    restart_local(&state, &instance).await?;
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

/// Drives the instance's service mounts volumes from, read back out of the
/// stored stack spec.
///
/// `check_placement` rejects a move to a node that does not implement these,
/// and that check has to happen here, before the migration is recorded. The
/// alternative is discovering it at `Switching`, which is the point of no
/// return: the volume's primary has already moved and the write that moves it
/// is the one that would fail.
///
/// An unknown stack or service yields an empty set, which reads as "no drive
/// requirements". That is the right answer for a stack with no volumes and
/// harmless for one that has since been removed, where the move fails on the
/// missing assignment instead.
fn required_drives(
    state: &cl_raft::ClusterState,
    instance: &InstanceId,
) -> std::collections::BTreeSet<String> {
    state
        .stacks
        .get(&instance.stack)
        .and_then(|stack| cl_config::StackFile::parse(&stack.spec).ok())
        .and_then(|file| file.services.get(&instance.service).map(Service::drives_used))
        .unwrap_or_default()
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

fn internal_update_path(instance: &InstanceId) -> String {
    format!("/api/instances/internal/{instance}/update")
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
        let node = assignment.node.clone();
        internal_peer::post_empty(&state, &node, &internal_update_path(&instance)).await?;
        return Ok(StatusCode::OK);
    }
    update_local(&state, instance).await?;
    Ok(StatusCode::OK)
}

/// Pulls the instance's current image on this node, records the new digest
/// (a leader-only raft write, hence still needing `submit` even when this is
/// not the leader), and restarts the container onto it.
async fn update_local(state: &AppState, instance: InstanceId) -> Result<(), ApiError> {
    let name = instance.container_name();
    let docker = state.docker().await?;
    let current = docker
        .inspect(&name)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::NotFound(format!("container {name} does not exist on this node")))?;

    let digest = docker.pull(&current.image).await.map_err(ApiError::internal)?;
    state
        .cluster
        .submit(ClusterRequest::SetImageDigest { instance, digest })
        .await
        .map_err(ApiError::not_leader)?;
    docker.restart(&name, Duration::from_secs(10)).await.map_err(ApiError::internal)
}

/// Node-to-node: updates this node's own container. The caller already
/// checked the assignment and the caller's own admin auth before forwarding
/// here.
async fn internal_update(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let instance = parse_id(&id)?;
    update_local(&state, instance).await?;
    Ok(StatusCode::OK)
}

#[derive(Deserialize, Serialize)]
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
) -> Result<Response, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.state().await;
    let assignment = cluster_state
        .assignments
        .get(&instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;
    if assignment.node != state.node_name {
        let node = assignment.node.clone();
        let qs = serde_urlencoded::to_string(&query).unwrap_or_default();
        let path = format!("{}?{qs}", internal_logs_path(&instance));
        return internal_peer::proxy_stream(&state, &node, &path).await;
    }
    serve_logs(&state, &instance, query).await
}

fn internal_logs_path(instance: &InstanceId) -> String {
    format!("/api/instances/internal/{instance}/logs")
}

async fn serve_logs(state: &AppState, instance: &InstanceId, query: LogsQuery) -> Result<Response, ApiError> {
    let opts = LogOptions {
        follow: query.follow,
        tail: query.tail.map(Tail::Lines).unwrap_or(Tail::Lines(200)),
        since: query.since,
    };
    let container_name = instance.container_name();
    let docker = state.docker().await?;
    let log_stream = async_stream::stream! {
        let stream = docker.logs(&container_name, opts);
        futures::pin_mut!(stream);
        while let Some(item) = futures::StreamExt::next(&mut stream).await {
            yield item;
        }
    };
    Ok(stream::container_log_events(log_stream).into_response())
}

async fn internal_logs(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogsQuery>,
) -> Result<Response, ApiError> {
    let instance = parse_id(&id)?;
    serve_logs(&state, &instance, query).await
}

fn internal_stats_path(instance: &InstanceId) -> String {
    format!("/api/instances/internal/{instance}/stats")
}

async fn stats(
    _identity: Identity,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.state().await;
    let assignment = cluster_state
        .assignments
        .get(&instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;
    if assignment.node != state.node_name {
        let node = assignment.node.clone();
        return internal_peer::proxy_stream(&state, &node, &internal_stats_path(&instance)).await;
    }
    serve_stats(&instance).await
}

async fn serve_stats(instance: &InstanceId) -> Result<Response, ApiError> {
    let samples = crate::docker_stats::stream(instance.container_name()).await?;
    Ok(stream::container_stat_events(samples).into_response())
}

async fn internal_stats(
    _auth: InternalAuth,
    State(_state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let instance = parse_id(&id)?;
    serve_stats(&instance).await
}
