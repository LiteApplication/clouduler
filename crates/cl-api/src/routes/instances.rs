//! `/api/instances`.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_config::Service;
use cl_docker::{Docker, ExecSpec, LogOptions, Tail};
use cl_raft::ids::InstanceId;
use cl_raft::state::DesiredState;
use cl_raft::Request as ClusterRequest;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
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
        .route("/api/rebalance", post(rebalance))
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
        // Node-to-node only, same as above: the cli resolves which node an
        // instance runs on itself (see `clouduler exec`) and connects to
        // that node directly, so there is no public forwarding counterpart
        // to this one yet.
        .route("/api/instances/internal/{id}/exec", get(internal_exec))
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
    /// The reverse proxy's own most recent active health check of this
    /// instance's service, when it publishes a route at all. `None` means
    /// either nothing is published for it or no check has landed yet, not
    /// that it's healthy — distinct from an empty `error` on a `Some`, which
    /// does mean healthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proxy: Option<ProxyHealthSummary>,
}

#[derive(Serialize, Deserialize)]
struct ProxyHealthSummary {
    host: String,
    checked_at: String,
    /// `None` when the last check reached the backend; `Some(reason)` in the
    /// same shape the proxy's own "bad gateway" response would have shown a
    /// real client, e.g. `"10.244.1.2:8080 unreachable: client error (SendRequest)"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
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
    let proxy = proxy_health_of(state, assignment);
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
        proxy,
    }
}

/// This node's own reverse proxy's latest check of the service this instance
/// belongs to, keyed `stack.service` since one route load balances every
/// replica of a service rather than publishing one per instance. `None` when
/// the service publishes no route, or no check has run for it yet.
fn proxy_health_of(state: &AppState, assignment: &cl_raft::Assignment) -> Option<ProxyHealthSummary> {
    let key = format!("{}.{}", assignment.instance.stack, assignment.instance.service);
    let health = state.proxy_health.get(&key)?;
    Some(ProxyHealthSummary {
        host: health.host,
        checked_at: humantime::format_rfc3339_seconds(health.checked_at).to_string(),
        error: health.error,
    })
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
    let nat_networks = config.nat_networks();
    drop(config);

    cl_sched::place(cluster_state, &desired, &served_drives, &nat_networks)
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
            proxy: None,
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

#[derive(Serialize)]
struct RebalanceMove {
    instance: String,
    from: String,
    to: String,
}

#[derive(Serialize)]
struct RebalanceResponse {
    moves: Vec<RebalanceMove>,
}

/// Actively relocate instances to smooth load across nodes, on operator
/// request only - see `cl_sched::rebalance::plan` for what it will and will
/// not move and why. Each move it decides on is submitted the same way the
/// ui's manual "move" button already does (`BeginMigration`), one at a time,
/// so the usual placement and drive checks still apply to every one of them;
/// a move rejected by a race with something else (a manual move, a redeploy)
/// just does not happen, it does not fail the rest of the batch.
async fn rebalance(_admin: RequireAdmin, State(state): State<AppState>) -> Result<Json<RebalanceResponse>, ApiError> {
    let cluster_state = state.cluster.state().await;
    let config = state.cluster_config.read().await;
    let colocate_proxy_hosts = config.proxy.dns_publish == cl_config::DnsPublishMode::OwningNode;
    let desired = cl_sched::desired_instances(
        &cluster_state,
        &config.egress_networks(),
        &config.port_gated_networks(),
        colocate_proxy_hosts,
    );
    let served_drives = config.served_drives();
    let nat_networks = config.nat_networks();
    drop(config);

    let planned = cl_sched::rebalance::plan(&cluster_state, &desired, &served_drives, &nat_networks);

    let mut moves = Vec::new();
    for m in planned {
        let drives = required_drives(&cluster_state, &m.instance);
        let response = state
            .cluster
            .submit(ClusterRequest::BeginMigration {
                instance: m.instance.clone(),
                to: m.to.clone(),
                drives,
            })
            .await
            .map_err(ApiError::not_leader)?;
        match response.error {
            None => moves.push(RebalanceMove {
                instance: m.instance.to_string(),
                from: m.from,
                to: m.to,
            }),
            Some(error) => warn!(instance = %m.instance, from = %m.from, to = %m.to, %error, "rebalance move was rejected"),
        }
    }
    Ok(Json(RebalanceResponse { moves }))
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

#[derive(Deserialize)]
struct ExecQuery {
    /// The command, json-encoded (`["sh","-c","ls"]`) so an argument can
    /// carry spaces or `&` without colliding with query-string parsing.
    cmd: String,
    #[serde(default)]
    tty: bool,
    user: Option<String>,
}

impl TryFrom<ExecQuery> for ExecSpec {
    type Error = ApiError;

    fn try_from(query: ExecQuery) -> Result<Self, Self::Error> {
        let cmd: Vec<String> =
            serde_json::from_str(&query.cmd).map_err(|e| ApiError::BadRequest(format!("cmd: {e}")))?;
        if cmd.is_empty() {
            return Err(ApiError::BadRequest("cmd must not be empty".to_string()));
        }
        Ok(ExecSpec { cmd, tty: query.tty, user: query.user })
    }
}

/// Node-to-node only: runs a command in this node's own container over a
/// websocket, binary frames each way, raw bytes with no framing beyond that
/// (the container's stdout/stderr are already combined by docker once `tty`
/// is set, which is the only mode the cli currently drives this with). The
/// caller already resolved which node to connect to from the replicated
/// assignment, the same way `internal_logs` and `internal_stats` are reached.
async fn internal_exec(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ExecQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let instance = parse_id(&id)?;
    let spec: ExecSpec = query.try_into()?;
    let docker = state.docker().await?;
    Ok(ws.on_upgrade(move |socket| bridge_exec(socket, docker, instance.container_name(), spec)))
}

async fn bridge_exec(mut socket: WebSocket, docker: Docker, container: String, spec: ExecSpec) {
    let mut session = match docker.exec(&container, spec).await {
        Ok(session) => session,
        Err(e) => {
            let _ = socket.send(Message::Text(format!("exec: {e}").into())).await;
            return;
        }
    };

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Binary(data))) => {
                        if session.input.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
            chunk = session.output.next() => {
                match chunk {
                    Some(Ok(chunk)) => {
                        if socket.send(Message::Binary(chunk.bytes)).await.is_err() {
                            break;
                        }
                    }
                    // The command exited, or docker itself errored: either
                    // way there is nothing left to relay.
                    Some(Err(e)) => {
                        let _ = socket.send(Message::Text(format!("exec: {e}").into())).await;
                        break;
                    }
                    None => break,
                }
            }
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}
