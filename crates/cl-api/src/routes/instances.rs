//! `/api/instances`.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_config::{Service, StackFile};
use cl_docker::{Docker, ExecSpec, LogOptions, Tail};
use cl_raft::ids::InstanceId;
use cl_raft::state::DesiredState;
use cl_raft::Request as ClusterRequest;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tracing::warn;

use crate::auth::internal::{InternalAuth, InternalBodyAuth};
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
        .route("/api/instances/{id}/actions", get(list_actions))
        .route("/api/instances/{id}/actions/{name}", post(run_action))
        .route("/api/instances/{id}/move", post(move_instance))
        .route("/api/instances/{id}/migration/retry", post(retry_migration))
        .route(
            "/api/instances/{id}/migration/dismiss",
            post(dismiss_migration),
        )
        .route("/api/rebalance", post(rebalance))
        .route("/api/rebalance/last", get(last_rebalance))
        .route("/api/instances/{id}/update", post(update))
        .route("/api/instances/{id}/logs", get(logs))
        .route("/api/instances/{id}/stats", get(stats))
        .route("/api/instances/events", get(instance_events))
        // Node-to-node only, HMAC authenticated: forwarded to when the instance lives on another node.
        .route(
            "/api/instances/internal/{id}/summary",
            get(internal_summary),
        )
        .route("/api/instances/internal/{id}/logs", get(internal_logs))
        .route("/api/instances/internal/{id}/stats", get(internal_stats))
        .route(
            crate::live_usage::NODE_LIVE_USAGE_PATH,
            get(internal_live_usage),
        )
        .route(
            "/api/instances/internal/cluster-live-usage",
            get(internal_cluster_live_usage),
        )
        .route("/api/instances/internal/{id}/update", post(internal_update))
        .route("/api/instances/internal/{id}/pull", post(internal_pull))
        .route(
            "/api/instances/internal/{id}/replace",
            post(internal_replace),
        )
        .route(
            "/api/instances/internal/{id}/remove-container",
            post(internal_remove_container),
        )
        // Node-to-node only: the cli resolves the instance's node itself, so there is no public forwarding counterpart.
        .route("/api/instances/internal/{id}/exec", get(internal_exec))
        // Node-to-node only: `clouduler capture` connects here directly.
        .route(
            "/api/instances/internal/{id}/capture",
            get(internal_capture),
        )
        .route(
            "/api/instances/internal/{id}/exec-run",
            post(internal_exec_run),
        )
        .route("/api/instances/internal/{id}/file", get(internal_file))
}

#[derive(Serialize, Deserialize)]
pub(crate) struct InstanceSummary {
    pub(crate) id: String,
    pub(crate) node: String,
    pub(crate) desired: String,
    pub(crate) state: String,
    pub(crate) running: Option<bool>,
    pub(crate) health: String,
    pub(crate) image: Option<String>,
    pub(crate) image_digest: Option<String>,
    /// Registry digest of what the container is actually running, vs `image_digest`
    /// (the pin). `None` when remote, no container, or a locally built image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) running_digest: Option<String>,
    pub(crate) started_at: Option<String>,
    pub(crate) migration: Option<MigrationSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) migration_problem: Option<crate::migration_problem::MigrationProblem>,
    /// What is happening now (redeploy sub-step or migration phase) with a
    /// `(step, total)` pair for a progress bar; see `compose_progress`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) step: Option<(u8, u8)>,
    /// Set only when `state` is `"pending_placement"`: why no node fits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) placement_error: Option<String>,
    /// The reverse proxy's own most recent active health check of this
    /// instance's service, when it publishes a route at all. `None` means
    /// either nothing is published for it or no check has landed yet, not
    /// that it's healthy — distinct from an empty `error` on a `Some`, which
    /// does mean healthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) proxy: Option<ProxyHealthSummary>,
    // Everything below crosses nodes inside `internal_summary`, so each is `default`:
    // an older peer leaves them out.
    /// Exit code of the container's last run, when it has stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) oom_killed: bool,
    #[serde(default)]
    pub(crate) restart_count: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) restart_limit: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) memory_limit_bytes: Option<i64>,
    #[serde(default)]
    pub(crate) crash_moves: u32,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ProxyHealthSummary {
    pub(crate) host: String,
    pub(crate) checked_at: String,
    /// `None` when the last check reached the backend; else the reason, shaped like the proxy's "bad gateway" body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct MigrationSummary {
    pub(crate) to: String,
    pub(crate) phase: String,
}

/// State names the ui and test harness match on. Strings, not an enum: "cannot see it from here" differs from "not running".
const STATE_RUNNING: &str = "running";
const STATE_STOPPED: &str = "stopped";
const STATE_STOPPING: &str = "stopping";
const STATE_RESTARTING: &str = "restarting";
const STATE_EXITED: &str = "exited";
const STATE_PULLING: &str = "pulling";
const STATE_WAITING: &str = "waiting";
const STATE_PENDING_PLACEMENT: &str = "pending_placement";
const STATE_UNKNOWN: &str = "unknown";

fn internal_summary_path(instance: &InstanceId) -> String {
    format!("/api/instances/internal/{instance}/summary")
}

/// Local instances come from docker, remote ones from the owning node over the internal api;
/// an unreachable node degrades to `summarize_local`'s unknown answer instead of failing the list.
pub(crate) async fn summarize(
    state: &AppState,
    cluster_state: &cl_raft::ClusterState,
    assignment: &cl_raft::Assignment,
) -> InstanceSummary {
    if assignment.node == state.node_name {
        return summarize_local(state, cluster_state, assignment).await;
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
            summarize_local(state, cluster_state, assignment).await
        }
    }
}

/// Docker inspection, valid only on the node running the container; also the "unknown" answer for remote assignments.
async fn summarize_local(
    state: &AppState,
    cluster_state: &cl_raft::ClusterState,
    assignment: &cl_raft::Assignment,
) -> InstanceSummary {
    let local = assignment.node == state.node_name;
    // Unreachable docker reads as unknown, not "stopped".
    let container = match (local, state.docker.get().await) {
        (true, Some(docker)) => docker
            .inspect(&assignment.instance.container_name())
            .await
            .ok()
            .flatten(),
        _ => None,
    };

    let running = container.as_ref().map(|c| c.running);
    // Looked up by image id, not tag: the tag has usually moved on, which is the drift this exposes.
    let running_digest = match (&container, state.docker.get().await) {
        (Some(c), Some(docker)) => {
            let repository = cl_docker::ImageRef::parse(&c.image).repository;
            match c.image_digest.as_deref() {
                Some(image_id) => docker
                    .digest_of_image(image_id, &repository)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            }
        }
        _ => None,
    };
    let proxy = proxy_health_of(state, assignment);
    let waiting_for = if local && container.is_none() {
        cl_sched::deps::unmet_dependency(cluster_state, &assignment.instance)
    } else {
        None
    };
    let instance_state = if !local {
        STATE_UNKNOWN
    } else {
        match &container {
            Some(c) => run_state_to_summary(c.status, assignment.desired),
            None if state
                .pulling
                .read()
                .await
                .contains_key(&assignment.instance) =>
            {
                STATE_PULLING
            }
            None if assignment.desired == DesiredState::Running && waiting_for.is_some() => {
                STATE_WAITING
            }
            None => STATE_STOPPED,
        }
    };

    let migration_sync = assignment.migration.as_ref().and_then(|_| {
        state
            .migration_sync
            .read()
            .get(&assignment.instance)
            .copied()
    });
    // Replicated, so real even when the node could not be asked.
    let migration_problem = crate::migration_problem::classify(cluster_state, &assignment.instance);
    let (mut reason, step) = compose_progress(
        cluster_state.instance_updates.get(&assignment.instance),
        assignment
            .migration
            .as_ref()
            .map(|m| (&m.phase, migration_sync)),
    );

    if let (Some(problem), Some(m)) = (&migration_problem, &assignment.migration) {
        reason = Some(format!("move to {} failed: {}", m.to, problem.title));
    }

    if instance_state == STATE_WAITING {
        reason = waiting_for.map(|dep| format!("waiting for {dep}"));
    }

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
        running_digest,
        started_at: container.as_ref().and_then(|c| c.started_at.clone()),
        migration: assignment.migration.as_ref().map(|m| MigrationSummary {
            to: m.to.clone(),
            phase: phase_name(&m.phase).to_string(),
        }),
        migration_problem,
        placement_error: None,
        proxy,
        reason,
        step,
        exit_code: container.as_ref().and_then(|c| c.exit_code),
        oom_killed: container.as_ref().is_some_and(|c| c.oom_killed),
        restart_count: container.as_ref().map_or(0, |c| c.restart_count),
        restart_limit: container.as_ref().and_then(|c| c.restart_limit),
        finished_at: container.as_ref().and_then(|c| c.finished_at.clone()),
        memory_limit_bytes: container.as_ref().and_then(|c| c.memory_limit_bytes),
        crash_moves: cluster_state
            .crash_moves
            .get(&assignment.instance)
            .map_or(0, |m| m.len() as u32),
    }
}

/// Keyed `stack.service`: one route load balances all replicas of a service.
fn proxy_health_of(
    state: &AppState,
    assignment: &cl_raft::Assignment,
) -> Option<ProxyHealthSummary> {
    let key = format!(
        "{}.{}",
        assignment.instance.stack, assignment.instance.service
    );
    let health = state.proxy_health.get(&key)?;
    Some(ProxyHealthSummary {
        host: health.host,
        checked_at: humantime::format_rfc3339_seconds(health.checked_at).to_string(),
        error: health.error,
    })
}

/// `Exited` means "crashed" when the cluster wanted it running, "stopped" otherwise.
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

fn compose_progress(
    update: Option<&cl_raft::InstanceUpdate>,
    migration: Option<(
        &cl_raft::state::MigrationPhase,
        Option<cl_drive::MigrationSyncProgress>,
    )>,
) -> (Option<String>, Option<(u8, u8)>) {
    if let Some((phase, sync_progress)) = migration {
        return compose_migration_reason(phase, sync_progress);
    }
    let Some(update) = update else {
        return (None, None);
    };
    use cl_raft::UpdatePhase;
    let (label, step) = match update.phase {
        UpdatePhase::Queued => return (Some("update queued".to_string()), None),
        UpdatePhase::Started => return (None, None),
        UpdatePhase::Failed => {
            let error = update.error.as_deref().unwrap_or("unknown error");
            return (Some(format!("update failed: {error}")), None);
        }
        UpdatePhase::Pulling => ("downloading image", 1),
        UpdatePhase::Stopping => ("stopping old container", 2),
        UpdatePhase::Starting => ("starting new container", 3),
    };
    (
        Some(format!(
            "updating ({label}, step {step}/{UPDATE_TOTAL_STEPS})"
        )),
        Some((step, UPDATE_TOTAL_STEPS)),
    )
}

const UPDATE_TOTAL_STEPS: u8 = 4;

/// `Failed` carries a reason, kept out of the name; it lives in `migration_problem`.
fn phase_name(phase: &cl_raft::state::MigrationPhase) -> &'static str {
    use cl_raft::state::MigrationPhase::*;
    match phase {
        Stopping => "stopping",
        Syncing => "syncing",
        Verifying => "verifying",
        Switching => "switching",
        Starting => "starting",
        Done => "done",
        Failed { .. } => "failed",
    }
}

fn compose_migration_reason(
    phase: &cl_raft::state::MigrationPhase,
    sync_progress: Option<cl_drive::MigrationSyncProgress>,
) -> (Option<String>, Option<(u8, u8)>) {
    if *phase == cl_raft::state::MigrationPhase::Syncing {
        if let Some(p) = sync_progress {
            return (
                Some(format!(
                    "migrating (syncing volume {}/{}, {}/{} files)",
                    p.volume_index, p.volume_count, p.files_done, p.files_total
                )),
                Some((p.volume_index as u8, p.volume_count as u8)),
            );
        }
    }
    match phase {
        cl_raft::state::MigrationPhase::Done => (None, None),
        cl_raft::state::MigrationPhase::Failed { .. } => (Some("move failed".to_string()), None),
        _ => (Some(format!("migrating ({})", phase_name(phase))), None),
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;
    use cl_raft::state::MigrationPhase;

    fn update(phase: cl_raft::UpdatePhase) -> cl_raft::InstanceUpdate {
        cl_raft::InstanceUpdate {
            node: "n1".into(),
            phase,
            started_at: 0,
            at: 0,
            error: None,
        }
    }

    #[test]
    fn update_reason_names_the_step() {
        let (reason, step) = compose_progress(Some(&update(cl_raft::UpdatePhase::Stopping)), None);
        assert_eq!(
            reason.as_deref(),
            Some("updating (stopping old container, step 2/4)")
        );
        assert_eq!(step, Some((2, 4)));
    }

    #[test]
    fn a_started_update_is_steady_state() {
        assert_eq!(
            compose_progress(Some(&update(cl_raft::UpdatePhase::Started)), None),
            (None, None)
        );
    }

    #[test]
    fn migration_reason_without_sync_progress_names_just_the_phase() {
        let (reason, step) = compose_migration_reason(&MigrationPhase::Verifying, None);
        assert_eq!(reason.as_deref(), Some("migrating (verifying)"));
        assert_eq!(step, None);
    }

    #[test]
    fn migration_reason_with_sync_progress_names_the_volume_and_file_counts() {
        let progress = cl_drive::MigrationSyncProgress {
            volume_index: 1,
            volume_count: 2,
            files_done: 5,
            files_total: 12,
        };
        let (reason, step) = compose_migration_reason(&MigrationPhase::Syncing, Some(progress));
        assert_eq!(
            reason.as_deref(),
            Some("migrating (syncing volume 1/2, 5/12 files)")
        );
        assert_eq!(step, Some((1, 2)));
    }
}

pub(crate) async fn progress_snapshot(
    state: &AppState,
    cluster_state: &cl_raft::ClusterState,
) -> Vec<(String, crate::stream::InstanceProgressEvent)> {
    let summaries = futures::future::join_all(
        cluster_state
            .assignments
            .values()
            .map(|a| summarize(state, cluster_state, a)),
    )
    .await;
    summaries
        .into_iter()
        .map(|s| {
            (
                s.id.clone(),
                crate::stream::InstanceProgressEvent {
                    instance_id: s.id,
                    state: s.state,
                    reason: s.reason,
                    step: s.step,
                },
            )
        })
        .collect()
}

async fn instance_events(_identity: Identity, State(state): State<AppState>) -> impl IntoResponse {
    stream::instance_progress_events(state)
}

async fn list(_identity: Identity, State(state): State<AppState>) -> Json<Vec<InstanceSummary>> {
    Json(list_all(&state).await)
}

pub(crate) async fn list_all(state: &AppState) -> Vec<InstanceSummary> {
    let cluster_state = state.cluster.snapshot().await;
    // Concurrent so one unreachable node (`FETCH_TIMEOUT`) does not serialize the whole list.
    let mut out = futures::future::join_all(
        cluster_state
            .assignments
            .values()
            .map(|a| summarize(state, &cluster_state, a)),
    )
    .await;
    out.extend(pending_placements(state, &cluster_state).await);
    out
}

/// Computed live from the scheduler's placement logic; nothing in `assignments` represents these.
async fn pending_placements(
    state: &AppState,
    cluster_state: &cl_raft::ClusterState,
) -> Vec<InstanceSummary> {
    let config = state.cluster_config.read().await;
    let colocate_proxy_hosts = config.proxy.dns_publish == cl_config::DnsPublishMode::OwningNode;
    let desired = cl_sched::desired_instances(
        cluster_state,
        &config.egress_networks(),
        &config.port_gated_networks(),
        colocate_proxy_hosts,
    );
    let served_drives = config.served_drives();
    let everywhere_networks = config.everywhere_networks();
    drop(config);

    cl_sched::place(
        cluster_state,
        &desired,
        &served_drives,
        &everywhere_networks,
    )
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
        running_digest: None,
        started_at: None,
        migration: None,
        migration_problem: None,
        reason: None,
        step: None,
        placement_error: Some(u.reason),
        proxy: None,
        exit_code: None,
        oom_killed: false,
        restart_count: 0,
        restart_limit: None,
        finished_at: None,
        memory_limit_bytes: None,
        crash_moves: 0,
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
    let cluster_state = state.cluster.snapshot().await;
    let assignment = cluster_state
        .assignments
        .get(&instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;

    Ok(Json(summarize(&state, &cluster_state, assignment).await))
}

/// Always answers about this node's docker; the caller already checked the assignment.
async fn internal_summary(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<InstanceSummary>, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.snapshot().await;
    let assignment = cluster_state
        .assignments
        .get(&instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;
    Ok(Json(
        summarize_local(&state, &cluster_state, assignment).await,
    ))
}

/// Rerolls placement rather than restarting in place: moves the instance to wherever it
/// would land if freshly scheduled now (possibly the same node). An in-place restart would
/// retry forever on a node that can no longer host it.
///
/// Done inline, not by unassigning and waiting for the scheduler: the effect is immediate,
/// and `Unassign` would drop the `image_digest` pin (`Assign` resets it to `None`), letting a
/// pinned instance drift to whatever its tag now resolves to.
async fn restart(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    restart_instance(&state, parse_id(&id)?).await?;
    Ok(StatusCode::OK)
}

pub(crate) async fn restart_instance(
    state: &AppState,
    instance: InstanceId,
) -> Result<(), ApiError> {
    let id = instance.to_string();
    let cluster_state = state.cluster.snapshot().await;
    let assignment = cluster_state
        .assignments
        .get(&instance)
        .cloned()
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;

    let config = state.cluster_config.read().await;
    let colocate_proxy_hosts = config.proxy.dns_publish == cl_config::DnsPublishMode::OwningNode;
    let desired = cl_sched::desired_instances(
        &cluster_state,
        &config.egress_networks(),
        &config.port_gated_networks(),
        colocate_proxy_hosts,
    );
    let served_drives = config.served_drives();
    let everywhere_networks = config.everywhere_networks();
    drop(config);

    let item = desired
        .into_iter()
        .find(|d| d.id == instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;

    // Placement leaves a valid assignment alone, so judge against a copy with it cleared.
    let mut probe = (*cluster_state).clone();
    probe.assignments.remove(&instance);
    let outcome = cl_sched::place::place(
        &probe,
        std::slice::from_ref(&item),
        &served_drives,
        &everywhere_networks,
    );
    let target = outcome
        .decisions
        .into_iter()
        .find_map(|d| match d {
            cl_sched::place::Decision::Place { node, .. } => Some(node),
            cl_sched::place::Decision::Remove { .. } => None,
        })
        .ok_or_else(|| {
            ApiError::Conflict("no node in the cluster can run this instance right now".to_string())
        })?;

    // Unassign + Assign to the same node leaves replicated state unchanged, so the node would
    // see nothing to do; removing the container makes reconcile rebuild it.
    if target == assignment.node {
        return remove_container_on(state, &instance, &assignment.node).await;
    }

    let response = state
        .cluster
        .submit(ClusterRequest::Unassign {
            instance: instance.clone(),
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }

    let response = state
        .cluster
        .submit(ClusterRequest::Assign {
            instance: instance.clone(),
            node: target,
            drives: item.drives,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }

    if let Some(digest) = assignment.image_digest {
        let response = state
            .cluster
            .submit(ClusterRequest::SetImageDigest { instance, digest })
            .await
            .map_err(ApiError::not_leader)?;
        if let Some(msg) = response.error {
            return Err(ApiError::Conflict(msg));
        }
    }

    Ok(())
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
    let cluster_state = state.cluster.snapshot().await;
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

async fn retry_migration(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.snapshot().await;
    let to = failed_move_target(&cluster_state, &instance)?;
    let drives = required_drives(&cluster_state, &instance);
    let response = state
        .cluster
        .submit(ClusterRequest::BeginMigration {
            instance,
            to,
            drives,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::OK)
}

async fn dismiss_migration(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let instance = parse_id(&id)?;
    let response = state
        .cluster
        .submit(ClusterRequest::ClearMigration { instance })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::OK)
}

fn failed_move_target(
    state: &cl_raft::ClusterState,
    instance: &InstanceId,
) -> Result<String, ApiError> {
    let assignment = state
        .assignments
        .get(instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {instance}")))?;
    match &assignment.migration {
        Some(m) if matches!(m.phase, cl_raft::state::MigrationPhase::Failed { .. }) => {
            Ok(m.to.clone())
        }
        Some(_) => Err(ApiError::Conflict(format!("{instance} is still moving"))),
        None => Err(ApiError::Conflict(format!("{instance} has no failed move"))),
    }
}

/// Drives the instance's service mounts, read from the stored stack spec.
///
/// Checked before the migration is recorded: discovering a missing drive at `Switching`
/// is past the point of no return (the volume's primary has already moved). An unknown
/// stack or service yields an empty set; a removed stack fails on the missing assignment instead.
fn required_drives(
    state: &cl_raft::ClusterState,
    instance: &InstanceId,
) -> std::collections::BTreeSet<String> {
    state
        .stacks
        .get(&instance.stack)
        .and_then(|stack| cl_config::StackFile::parse(&stack.spec).ok())
        .and_then(|file| {
            file.services
                .get(&instance.service)
                .map(Service::drives_used)
        })
        .unwrap_or_default()
}

#[derive(Serialize, Deserialize)]
struct RebalanceMove {
    instance: String,
    from: String,
    to: String,
}

#[derive(Serialize)]
struct RebalanceResponse {
    moves: Vec<RebalanceMove>,
}

/// Reserved config holding the latest rebalance, so any node can answer `GET /api/rebalance/last`.
const LAST_REBALANCE_CONFIG: &str = "_clouduler_last_rebalance";

#[derive(Serialize, Deserialize)]
struct LastRebalance {
    at: i64,
    moves: Vec<RebalanceMove>,
}

async fn last_rebalance(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Option<LastRebalance>> {
    let cluster_state = state.cluster.snapshot().await;
    Json(
        cluster_state
            .configs
            .get(LAST_REBALANCE_CONFIG)
            .and_then(|c| serde_json::from_str(&c.content).ok()),
    )
}

/// Operator-requested rebalance; see `cl_sched::rebalance::plan`. Each move is a
/// `BeginMigration` submitted one at a time like the ui's "move" button, so placement and
/// drive checks apply; a move lost to a race does not fail the rest of the batch.
async fn rebalance(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> Result<Json<RebalanceResponse>, ApiError> {
    let cluster_state = state.cluster.snapshot().await;
    let config = state.cluster_config.read().await;
    let colocate_proxy_hosts = config.proxy.dns_publish == cl_config::DnsPublishMode::OwningNode;
    let desired = cl_sched::desired_instances(
        &cluster_state,
        &config.egress_networks(),
        &config.port_gated_networks(),
        colocate_proxy_hosts,
    );
    let served_drives = config.served_drives();
    let everywhere_networks = config.everywhere_networks();
    drop(config);

    let live = crate::live_usage::gather(&state).await;
    let planned = cl_sched::rebalance::plan(
        &cluster_state,
        &desired,
        &served_drives,
        &everywhere_networks,
        &live,
    );

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
            Some(error) => {
                warn!(instance = %m.instance, from = %m.from, to = %m.to, %error, "rebalance move was rejected")
            }
        }
    }
    let record = LastRebalance {
        at: crate::auth::oauth::now_secs(),
        moves,
    };
    // Best effort: only the ui's "last rebalance" line depends on it.
    if let Ok(content) = serde_json::to_string(&record) {
        if let Err(e) = state
            .cluster
            .submit(ClusterRequest::PutConfig {
                name: LAST_REBALANCE_CONFIG.to_string(),
                content,
                owner: None,
            })
            .await
        {
            warn!(error = %e, "could not record the rebalance");
        }
    }
    Ok(Json(RebalanceResponse {
        moves: record.moves,
    }))
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
    update_instance_guarded(&state, parse_id(&id)?).await?;
    Ok(StatusCode::OK)
}

/// `update_instance`, refusing with a conflict when one is already running.
pub(crate) async fn update_instance_guarded(
    state: &AppState,
    instance: InstanceId,
) -> Result<(), ApiError> {
    // A second update would pull again and remove the container the first is waiting on.
    // The bulk path (`updates::apply_selection`) queues its own record first.
    let in_flight = state
        .cluster
        .state()
        .await
        .instance_updates
        .get(&instance)
        .is_some_and(|u| !u.phase.is_terminal());
    if in_flight {
        return Err(ApiError::Conflict(format!(
            "an update of {instance} is already in progress"
        )));
    }
    update_instance(state, instance).await
}

/// Re-pins `digest` and removes the container so reconcile rebuilds on it: how a failed rolling update undoes itself.
pub(crate) async fn roll_back_instance(
    state: &AppState,
    instance: &InstanceId,
    digest: String,
) -> Result<(), ApiError> {
    state
        .cluster
        .submit(ClusterRequest::SetImageDigest {
            instance: instance.clone(),
            digest,
        })
        .await
        .map_err(ApiError::not_leader)?;
    let node = state
        .cluster
        .state()
        .await
        .assignments
        .get(instance)
        .map(|a| a.node.clone())
        .ok_or_else(|| ApiError::NotFound(format!("no instance {instance}")))?;
    remove_container_on(state, instance, &node).await
}

pub(crate) async fn remove_container_on(
    state: &AppState,
    instance: &InstanceId,
    node: &str,
) -> Result<(), ApiError> {
    if node != state.node_name {
        let path = format!("/api/instances/internal/{instance}/remove-container");
        return internal_peer::post_empty(state, node, &path).await;
    }
    remove_container(state, instance).await
}

async fn remove_container(state: &AppState, instance: &InstanceId) -> Result<(), ApiError> {
    let name = instance.container_name();
    let docker = state.docker().await?;
    // Reconcile's `failover.stop_grace`, not docker's 10s, which is short for a database.
    let stop_grace = state.cluster_config.read().await.failover.stop_grace;
    docker.stop(&name, stop_grace).await.ok();
    docker
        .remove(&name, true)
        .await
        .map_err(ApiError::internal)?;
    Ok(())
}

async fn internal_remove_container(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    remove_container(&state, &parse_id(&id)?).await?;
    Ok(StatusCode::OK)
}

/// Shared with `routes::updates`, which drives it across a list of instances.
pub(crate) async fn update_instance(
    state: &AppState,
    instance: InstanceId,
) -> Result<(), ApiError> {
    let node = owning_node(state, &instance).await?;
    if node != state.node_name {
        return internal_peer::post_empty(state, &node, &internal_update_path(&instance)).await;
    }
    update_local(state, instance).await
}

/// First half of an update: downloads the image and returns the digest to pin; the old
/// container keeps serving. Split so a rolling update can pull the next instance during a restart.
pub(crate) async fn pull_instance(
    state: &AppState,
    instance: &InstanceId,
) -> Result<String, ApiError> {
    let node = owning_node(state, instance).await?;
    if node != state.node_name {
        let path = format!("/api/instances/internal/{instance}/pull");
        let pulled: Pulled = internal_peer::post_json(state, &node, &path).await?;
        return Ok(pulled.digest);
    }
    pull_local(state, instance).await
}

pub(crate) async fn replace_instance(
    state: &AppState,
    instance: &InstanceId,
    digest: String,
) -> Result<(), ApiError> {
    let node = owning_node(state, instance).await?;
    if node != state.node_name {
        let query = serde_urlencoded::to_string(Pulled { digest }).unwrap_or_default();
        let path = format!("/api/instances/internal/{instance}/replace?{query}");
        return internal_peer::post_empty(state, &node, &path).await;
    }
    replace_local(state, instance, digest).await
}

pub(crate) async fn owning_node(
    state: &AppState,
    instance: &InstanceId,
) -> Result<String, ApiError> {
    state
        .cluster
        .state()
        .await
        .assignments
        .get(instance)
        .map(|a| a.node.clone())
        .ok_or_else(|| ApiError::NotFound(format!("no instance {instance}")))
}

#[derive(Deserialize, Serialize)]
struct Pulled {
    digest: String,
}

/// Pulls the image here, records the digest (a raft write via `submit`), and removes the old
/// container so reconcile builds a new one on the new bits.
///
/// Removing rather than restarting is the point: `docker restart` reuses the container, bound
/// to its original image id, so the update would silently do nothing. Nor does a changed pin
/// trigger `Action::Recreate`, whose revision comes from the stack revision and mount shape
/// (`revision_with_mounts`), not the image.
///
/// Ordering against reconcile's `Action::Create` does not matter: if it runs before the pin
/// applies, it creates from the tag, which the pull just repointed at the same digest.
/// Each step is recorded in `ClusterState::instance_updates`; the reconciler records `Started`.
async fn update_local(state: &AppState, instance: InstanceId) -> Result<(), ApiError> {
    let progress = cl_sched::UpdateReporter::new(state.cluster.clone(), state.node_name.clone());
    let result = update_local_steps(state, &instance, &progress).await;
    if let Err(e) = &result {
        progress
            .report(&instance, cl_raft::UpdatePhase::Failed, Some(e.to_string()))
            .await;
    }
    result
}

async fn update_local_steps(
    state: &AppState,
    instance: &InstanceId,
    progress: &cl_sched::UpdateReporter,
) -> Result<(), ApiError> {
    let digest = pull_steps(state, instance, progress).await?;
    replace_steps(state, instance, digest, progress).await
}

async fn pull_local(state: &AppState, instance: &InstanceId) -> Result<String, ApiError> {
    let progress = cl_sched::UpdateReporter::new(state.cluster.clone(), state.node_name.clone());
    let result = pull_steps(state, instance, &progress).await;
    if let Err(e) = &result {
        progress
            .report(instance, cl_raft::UpdatePhase::Failed, Some(e.to_string()))
            .await;
    }
    result
}

async fn replace_local(
    state: &AppState,
    instance: &InstanceId,
    digest: String,
) -> Result<(), ApiError> {
    let progress = cl_sched::UpdateReporter::new(state.cluster.clone(), state.node_name.clone());
    let result = replace_steps(state, instance, digest, &progress).await;
    if let Err(e) = &result {
        progress
            .report(instance, cl_raft::UpdatePhase::Failed, Some(e.to_string()))
            .await;
    }
    result
}

async fn pull_steps(
    state: &AppState,
    instance: &InstanceId,
    progress: &cl_sched::UpdateReporter,
) -> Result<String, ApiError> {
    use cl_raft::UpdatePhase;
    let docker = state.docker().await?;
    // A container that is absent (e.g. a failed migration left it unstartable on a pinned
    // digest the registry no longer serves) is still updatable.

    // The declared reference (`nginx:1.25`), not the running container's image: once pinned
    // that is `repo@sha256:...`, and pulling a digest is a no-op forever. See `updates::declared_image`.
    let cluster_state = state.cluster.snapshot().await;
    let image = cluster_state
        .stacks
        .get(&instance.stack)
        .and_then(|stack| StackFile::parse(&stack.spec).ok())
        .and_then(|file| {
            file.services
                .get(&instance.service)
                .map(|s| s.image.clone())
        })
        .ok_or_else(|| ApiError::NotFound(format!("no stack service for instance {instance}")))?;
    drop(cluster_state);

    progress.report(instance, UpdatePhase::Pulling, None).await;
    let registries = super::registries::load(state).await;
    // Marked as the reconcile loop marks its pulls: docker inspect cannot tell "fetching"
    // from "about to be created", and `apply` returns before the pull finishes.
    cl_sched::pull_started(&state.pulling, instance).await;
    let pulled = docker.pull_as(&image, registries.for_image(&image)).await;
    cl_sched::pull_finished(&state.pulling, instance).await;
    pulled.map_err(ApiError::internal)
}

async fn replace_steps(
    state: &AppState,
    instance: &InstanceId,
    digest: String,
    progress: &cl_sched::UpdateReporter,
) -> Result<(), ApiError> {
    use cl_raft::UpdatePhase;
    let name = instance.container_name();
    let docker = state.docker().await?;
    let running = docker.inspect(&name).await.map_err(ApiError::internal)?;
    let Some(running) = running else {
        state
            .cluster
            .submit(ClusterRequest::SetImageDigest {
                instance: instance.clone(),
                digest,
            })
            .await
            .map_err(ApiError::not_leader)?;
        progress.report(instance, UpdatePhase::Started, None).await;
        return Ok(());
    };

    // Remember what is running so a failed update can restore it. An unpinned instance has no
    // recorded pin and the pull moved its tag, so resolve the running image's digest (by image
    // id, untouched by the pull) and pin that first.
    let pinned_before = state
        .cluster
        .state()
        .await
        .assignments
        .get(instance)
        .and_then(|a| a.image_digest.clone());
    if pinned_before.is_none() {
        let repository = cl_docker::ImageRef::parse(&running.image).repository;
        if let Some(image_id) = running.image_digest.as_deref() {
            if let Ok(Some(old)) = docker.digest_of_image(image_id, &repository).await {
                state
                    .cluster
                    .submit(ClusterRequest::SetImageDigest {
                        instance: instance.clone(),
                        digest: old,
                    })
                    .await
                    .map_err(ApiError::not_leader)?;
            }
        }
    }
    state
        .cluster
        .submit(ClusterRequest::SetImageDigest {
            instance: instance.clone(),
            digest,
        })
        .await
        .map_err(ApiError::not_leader)?;

    progress.report(instance, UpdatePhase::Stopping, None).await;
    // Reconcile's `failover.stop_grace`, not docker's 10s, which is short for a database.
    let stop_grace = state.cluster_config.read().await.failover.stop_grace;
    docker.stop(&name, stop_grace).await.ok();
    docker
        .remove(&name, true)
        .await
        .map_err(ApiError::internal)?;
    progress.report(instance, UpdatePhase::Starting, None).await;
    Ok(())
}

async fn internal_pull(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Pulled>, ApiError> {
    let digest = pull_local(&state, &parse_id(&id)?).await?;
    Ok(Json(Pulled { digest }))
}

async fn internal_replace(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(pulled): Query<Pulled>,
) -> Result<StatusCode, ApiError> {
    replace_local(&state, &parse_id(&id)?, pulled.digest).await?;
    Ok(StatusCode::OK)
}

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
    let cluster_state = state.cluster.snapshot().await;
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

async fn serve_logs(
    state: &AppState,
    instance: &InstanceId,
    query: LogsQuery,
) -> Result<Response, ApiError> {
    let opts = LogOptions {
        follow: query.follow,
        tail: query.tail.map(Tail::Lines).unwrap_or(Tail::Lines(200)),
        since: query.since,
    };
    let container_name = instance.container_name();
    let docker = state.docker().await?;
    let log_stream = async_stream::stream! {
        let stream = docker.logs(&container_name, &opts);
        futures::pin_mut!(stream);
        while let Some(item) = futures::StreamExt::next(&mut stream).await {
            yield item;
        }
        if let Some(line) = docker.start_failure_line(&container_name).await {
            yield Ok(line);
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
    let cluster_state = state.cluster.snapshot().await;
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

/// This node's own live usage for rebalancing; a node only answers for itself.
async fn internal_live_usage(
    _auth: InternalAuth,
    State(state): State<AppState>,
) -> Json<crate::live_usage::NodeLiveUsage> {
    Json(crate::live_usage::local(&state).await)
}

/// Live usage of every reachable node, gathered by this node, for `clouduler rebalance`.
async fn internal_cluster_live_usage(
    _auth: InternalAuth,
    State(state): State<AppState>,
) -> Json<cl_sched::rebalance::LiveUsage> {
    Json(crate::live_usage::gather(&state).await)
}

#[derive(Deserialize)]
struct ExecQuery {
    /// JSON-encoded (`["sh","-c","ls"]`) so arguments can hold spaces or `&` safely in a query string.
    cmd: String,
    #[serde(default)]
    tty: bool,
    user: Option<String>,
}

impl TryFrom<ExecQuery> for ExecSpec {
    type Error = ApiError;

    fn try_from(query: ExecQuery) -> Result<Self, Self::Error> {
        let cmd: Vec<String> = serde_json::from_str(&query.cmd)
            .map_err(|e| ApiError::BadRequest(format!("cmd: {e}")))?;
        if cmd.is_empty() {
            return Err(ApiError::BadRequest("cmd must not be empty".to_string()));
        }
        Ok(ExecSpec {
            cmd,
            tty: query.tty,
            user: query.user,
        })
    }
}

/// Node-to-node only: runs a command over a websocket, raw binary frames each way
/// (stdout/stderr are combined by docker in `tty` mode, the only mode the cli uses).
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
            let _ = socket
                .send(Message::Text(format!("exec: {e}").into()))
                .await;
            return;
        }
    };

    // A non-interactive caller (piped stdin) hits stdin EOF almost immediately, often before any
    // output. That only means "no more input": close `session.input` and keep relaying output
    // rather than tearing down the session.
    let exec_id = session.id.clone();
    let mut input = Some(session.input);
    let mut finished = false;
    loop {
        tokio::select! {
            incoming = socket.recv(), if input.is_some() => {
                match incoming {
                    // An empty frame is the client's stdin EOF (`cl-cli` `exec::bridge`): half-close stdin,
                    // keep relaying output.
                    Some(Ok(Message::Binary(data))) if data.is_empty() => {
                        let _ = input.as_mut().unwrap().shutdown().await;
                        input = None;
                    }
                    Some(Ok(Message::Binary(data))) => {
                        if input.as_mut().unwrap().write_all(&data).await.is_err() {
                            input = None;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => input = None,
                }
            }
            chunk = session.output.next() => {
                match chunk {
                    Some(Ok(chunk)) => {
                        if socket.send(Message::Binary(chunk.bytes)).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        let _ = socket.send(Message::Text(format!("exec: {e}").into())).await;
                        break;
                    }
                    None => {
                        finished = true;
                        break;
                    }
                }
            }
        }
    }
    // Lets the client exit with the same code (`exit:<n>`; other text frames are errors).
    if finished {
        if let Ok(Some(code)) = docker.exec_exit_code(&exec_id).await {
            let _ = socket
                .send(Message::Text(format!("exit:{code}").into()))
                .await;
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

#[derive(Deserialize)]
struct CaptureQuery {
    /// Defaults to the overlay network `clouduler stack deploy` builds (`cl_sched::run::overlay_network_name`),
    /// the one the proxy forwards to.
    network: Option<String>,
    /// Raw BPF filter (`tcpdump` syntax), space-split; a filter needing quoting is unsupported.
    filter: Option<String>,
    /// Stream raw pcap bytes (`tcpdump -w -`) instead of text lines.
    #[serde(default)]
    pcap: bool,
}

/// Node-to-node only: runs `tcpdump` on this node's bridge for one of the stack's overlay
/// networks (`cl_net::vxlan::bridge_name`) and streams output over a websocket like `internal_exec`.
async fn internal_capture(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<CaptureQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.snapshot().await;
    let stack = cluster_state
        .stacks
        .get(&instance.stack)
        .ok_or_else(|| ApiError::NotFound(format!("stack {} not found", instance.stack)))?;
    let file = StackFile::parse(&stack.spec).map_err(|e| ApiError::Internal(e.to_string()))?;

    let network = match query.network {
        Some(n) => n,
        None => {
            let cfg = state.cluster_config.read().await;
            cl_sched::run::overlay_network_name(&file, &cfg)
                .ok_or_else(|| {
                    ApiError::BadRequest(format!(
                        "stack {} has no subnetted or public network to pick a default from; pass --network",
                        instance.stack
                    ))
                })?
                .to_string()
        }
    };
    if !file.networks.contains_key(&network) {
        return Err(ApiError::BadRequest(format!(
            "stack {} declares no network {network}",
            instance.stack
        )));
    }

    let overlay = cl_sched::run::overlay_name(&instance.stack, &network);
    let bridge = cl_net::vxlan::bridge_name(&overlay);
    Ok(ws.on_upgrade(move |socket| bridge_capture(socket, bridge, query.pcap, query.filter)))
}

async fn bridge_capture(mut socket: WebSocket, iface: String, pcap: bool, filter: Option<String>) {
    let mut cmd = Command::new("tcpdump");
    cmd.arg("-i").arg(&iface).arg("-nn").kill_on_drop(true);
    if pcap {
        cmd.arg("-w").arg("-");
    } else {
        cmd.arg("-l").arg("-U");
    }
    if let Some(filter) = &filter {
        cmd.args(filter.split_whitespace());
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let _ = socket
                .send(Message::Text(format!("tcpdump: {e}").into()))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    let mut stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    // Relayed on its own task: once stderr hits EOF a `select!` arm would spin on `Ok(0)` forever.
    let (err_tx, mut err_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut stderr = stderr;
        let mut buf = [0u8; 4096];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if err_tx
                        .send(String::from_utf8_lossy(&buf[..n]).into_owned())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    let mut buf = [0u8; 8192];
    let mut err_open = true;
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            n = stdout.read(&mut buf) => {
                match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let msg = if pcap { Message::Binary(buf[..n].to_vec().into()) } else { Message::Text(String::from_utf8_lossy(&buf[..n]).into_owned().into()) };
                        if socket.send(msg).await.is_err() {
                            break;
                        }
                    }
                }
            }
            // Disabled once the sender task ends, instead of spinning on a ready `None`.
            text = err_rx.recv(), if err_open => {
                match text {
                    Some(text) => {
                        if socket.send(Message::Text(format!("tcpdump: {text}").into())).await.is_err() {
                            break;
                        }
                    }
                    None => err_open = false,
                }
            }
        }
    }
    let _ = child.start_kill();
    let _ = socket.send(Message::Close(None)).await;
}

// Buffered inspection for the MCP layer. Public halves run locally or forward to the owning
// node. Nothing is redacted here: that is the MCP layer's job on the serving node.

/// Ceiling on one buffered command; the forwarding client's write timeout is also 300s.
const EXEC_RUN_MAX_SECS: u64 = 300;
const EXEC_RUN_DEFAULT_SECS: u64 = 30;
/// Ceiling per stream, stdin and file read: the answer travels as json into a model's context.
const MAX_BUFFERED_BYTES: usize = 1024 * 1024;
const EXEC_RUN_DEFAULT_OUTPUT: usize = 64 * 1024;
const FILE_READ_DEFAULT: usize = 256 * 1024;
const FILE_LIST_LIMIT: usize = 500;
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
const LOG_TAIL_LIMIT: u32 = 10_000;

fn default_exec_timeout() -> u64 {
    EXEC_RUN_DEFAULT_SECS
}

fn default_exec_output() -> usize {
    EXEC_RUN_DEFAULT_OUTPUT
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecRunRequest {
    pub(crate) cmd: Vec<String>,
    #[serde(default)]
    pub(crate) user: Option<String>,
    /// Written to stdin, which is then closed; without it stdin closes immediately.
    #[serde(default)]
    pub(crate) stdin: Option<String>,
    #[serde(default = "default_exec_timeout")]
    pub(crate) timeout_secs: u64,
    #[serde(default = "default_exec_output")]
    pub(crate) max_output: usize,
    /// Run in a one-off container replacing the instance's own for the duration; `stdin` unsupported.
    #[serde(default)]
    pub(crate) standalone: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecRunResult {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) exit_code: Option<i64>,
    pub(crate) timed_out: bool,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
    #[serde(default)]
    pub(crate) duration_ms: u64,
}

/// Output past the cap is still read and discarded so the command is not blocked on a full pipe.
struct CappedBuf {
    data: Vec<u8>,
    cap: usize,
    truncated: bool,
}

impl CappedBuf {
    fn new(cap: usize) -> Self {
        Self {
            data: Vec::new(),
            cap,
            truncated: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let room = self.cap.saturating_sub(self.data.len());
        if bytes.len() > room {
            self.truncated = true;
        }
        self.data.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    fn into_text(self) -> (String, bool) {
        (
            String::from_utf8_lossy(&self.data).into_owned(),
            self.truncated,
        )
    }
}

fn docker_error(e: cl_docker::DockerError) -> ApiError {
    match e {
        cl_docker::DockerError::NotFound(what) => ApiError::NotFound(what),
        other => ApiError::internal(other),
    }
}

/// Not a tty, so stdout and stderr stay separate. On timeout it stops reading but the
/// command keeps running: docker cannot kill an exec, so side effects may still land.
pub(crate) async fn exec_run_local(
    state: &AppState,
    instance: &InstanceId,
    req: ExecRunRequest,
) -> Result<ExecRunResult, ApiError> {
    if req.cmd.is_empty() {
        return Err(ApiError::BadRequest("cmd must not be empty".to_string()));
    }
    if req.stdin.as_deref().map_or(0, str::len) > MAX_BUFFERED_BYTES {
        return Err(ApiError::BadRequest(format!(
            "stdin is limited to {MAX_BUFFERED_BYTES} bytes"
        )));
    }

    let docker = state.docker().await?;
    if req.standalone {
        if req.stdin.is_some() {
            return Err(ApiError::BadRequest(
                "a standalone action takes no stdin".to_string(),
            ));
        }
        return exec_standalone(&docker, instance, req).await;
    }
    exec_run_docker(&docker, instance, req).await
}

const STANDALONE_STOP_GRACE: Duration = Duration::from_secs(30);

/// Runs `req.cmd` in a one-off copy with the instance stopped meanwhile. The copy exists the
/// whole time, which keeps the reconciler from restarting the instance (`Docker::action_holds`);
/// the instance is restarted at the end if it was running.
async fn exec_standalone(
    docker: &Docker,
    instance: &InstanceId,
    req: ExecRunRequest,
) -> Result<ExecRunResult, ApiError> {
    let main = instance.container_name();
    let name = format!("{main}-action");
    let main_state = docker
        .inspect(&main)
        .await
        .map_err(docker_error)?
        .ok_or_else(|| ApiError::NotFound(format!("container {main} does not exist")))?;
    if let Some(old) = docker.inspect(&name).await.map_err(docker_error)? {
        if matches!(
            old.status,
            cl_docker::RunState::Running | cl_docker::RunState::Created
        ) {
            return Err(ApiError::Conflict(format!(
                "an action is already running on {instance}"
            )));
        }
        docker.remove(&name, true).await.map_err(docker_error)?;
    }

    let started = std::time::Instant::now();
    let timeout = Duration::from_secs(req.timeout_secs.clamp(1, EXEC_RUN_MAX_SECS));
    let cap = req.max_output.clamp(1, MAX_BUFFERED_BYTES);
    // Created before the instance stops so the reconciler never sees a gap.
    docker
        .create_action_container(&main, &name, req.cmd, req.user)
        .await
        .map_err(docker_error)?;

    let result = run_standalone(docker, &main, &name, main_state.running, timeout, cap).await;

    if let Err(e) = docker.remove(&name, true).await {
        warn!(%instance, error = %e, "could not remove the action container");
    }
    if main_state.running {
        if let Err(e) = docker.start(&main).await {
            let up = matches!(docker.inspect(&main).await, Ok(Some(s)) if s.running);
            if !up {
                warn!(%instance, error = %e, "could not start the instance after its action; the reconciler will retry");
            }
        }
    }
    let mut result = result?;
    result.duration_ms = started.elapsed().as_millis() as u64;
    Ok(result)
}

async fn run_standalone(
    docker: &Docker,
    main: &str,
    name: &str,
    main_running: bool,
    timeout: Duration,
    cap: usize,
) -> Result<ExecRunResult, ApiError> {
    if main_running {
        docker
            .stop(main, STANDALONE_STOP_GRACE)
            .await
            .map_err(docker_error)?;
    }
    docker.start(name).await.map_err(docker_error)?;
    let (exit_code, timed_out) = match tokio::time::timeout(timeout, docker.wait(name)).await {
        Ok(Ok(code)) => (Some(code), false),
        Ok(Err(e)) => return Err(docker_error(e)),
        Err(_) => (None, true),
    };

    let mut out = CappedBuf::new(cap);
    let mut err = CappedBuf::new(cap);
    let mut logs = Box::pin(docker.logs(name, &LogOptions::default()));
    while let Some(line) = logs.next().await {
        let line = line.map_err(docker_error)?;
        let (buf, text) = match line.stream {
            cl_docker::LogStream::Stdout => (&mut out, line.text),
            cl_docker::LogStream::Stderr => (&mut err, line.text),
        };
        buf.push(text.as_bytes());
        buf.push(b"\n");
    }
    if let Some(line) = docker.start_failure_line(name).await {
        err.push(line.text.as_bytes());
        err.push(b"\n");
    }
    let (stdout, stdout_truncated) = out.into_text();
    let (stderr, stderr_truncated) = err.into_text();
    Ok(ExecRunResult {
        stdout,
        stderr,
        exit_code,
        timed_out,
        stdout_truncated,
        stderr_truncated,
        duration_ms: 0,
    })
}

async fn exec_run_docker(
    docker: &Docker,
    instance: &InstanceId,
    req: ExecRunRequest,
) -> Result<ExecRunResult, ApiError> {
    if req.cmd.is_empty() {
        return Err(ApiError::BadRequest("cmd must not be empty".to_string()));
    }
    let started = std::time::Instant::now();
    let stdin = req.stdin.unwrap_or_default().into_bytes();
    let timeout = Duration::from_secs(req.timeout_secs.clamp(1, EXEC_RUN_MAX_SECS));
    let cap = req.max_output.clamp(1, MAX_BUFFERED_BYTES);
    let session = docker
        .exec(
            &instance.container_name(),
            ExecSpec {
                cmd: req.cmd,
                tty: false,
                user: req.user,
            },
        )
        .await
        .map_err(docker_error)?;
    let exec_id = session.id;
    let mut output = session.output;

    // Own task: a command that fills stdout before reading all of stdin would deadlock this loop.
    let mut input = session.input;
    tokio::spawn(async move {
        if !stdin.is_empty() {
            let _ = input.write_all(&stdin).await;
        }
        let _ = input.shutdown().await;
    });

    let mut out = CappedBuf::new(cap);
    let mut err = CappedBuf::new(cap);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut timed_out = false;
    loop {
        match tokio::time::timeout_at(deadline, output.next()).await {
            Err(_) => {
                timed_out = true;
                break;
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err(docker_error(e)),
            Ok(Some(Ok(chunk))) => match chunk.stream {
                cl_docker::ExecStream::Stdout => out.push(&chunk.bytes),
                cl_docker::ExecStream::Stderr => err.push(&chunk.bytes),
            },
        }
    }

    let exit_code = if timed_out {
        None
    } else {
        docker.exec_exit_code(&exec_id).await.ok().flatten()
    };
    let (stdout, stdout_truncated) = out.into_text();
    let (stderr, stderr_truncated) = err.into_text();
    Ok(ExecRunResult {
        stdout,
        stderr,
        exit_code,
        timed_out,
        stdout_truncated,
        stderr_truncated,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

fn internal_exec_run_path(instance: &InstanceId) -> String {
    format!("/api/instances/internal/{instance}/exec-run")
}

/// See `exec_run_local` for the timeout caveat.
pub(crate) async fn exec_run(
    state: &AppState,
    instance: &InstanceId,
    req: ExecRunRequest,
) -> Result<ExecRunResult, ApiError> {
    let node = owning_node(state, instance).await?;
    if node == state.node_name {
        return exec_run_local(state, instance, req).await;
    }
    internal_peer::post_json_body(state, &node, &internal_exec_run_path(instance), &req).await
}

/// Takes a body, so the signed body hash is checked against what was read.
async fn internal_exec_run(
    auth: InternalBodyAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<ExecRunResult>, ApiError> {
    auth.check(&body)?;
    let req: ExecRunRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::BadRequest(format!("invalid exec-run request: {e}")))?;
    Ok(Json(exec_run_local(&state, &parse_id(&id)?, req).await?))
}

fn instance_service(
    state: &cl_raft::ClusterState,
    instance: &InstanceId,
) -> Result<Service, ApiError> {
    let stack = state
        .stacks
        .get(&instance.stack)
        .ok_or_else(|| ApiError::NotFound(format!("stack {} not found", instance.stack)))?;
    let file = StackFile::parse(&stack.spec).map_err(|e| ApiError::Internal(e.to_string()))?;
    file.services
        .get(&instance.service)
        .cloned()
        .ok_or_else(|| ApiError::NotFound(format!("no service {}", instance.service)))
}

#[derive(Serialize)]
struct ActionSummary {
    name: String,
    confirm: bool,
}

async fn list_actions(
    _identity: Identity,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ActionSummary>>, ApiError> {
    let service = instance_service(&state.cluster.state().await, &parse_id(&id)?)?;
    Ok(Json(
        service
            .x_clouduler
            .map(|x| {
                x.actions
                    .into_iter()
                    .filter(|(_, a)| a.show_in_ui)
                    .map(|(name, a)| ActionSummary {
                        name,
                        confirm: a.confirm,
                    })
                    .collect()
            })
            .unwrap_or_default(),
    ))
}

/// Only commands written in the stack file run: the caller picks a name, never a command line.
async fn run_action(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<ExecRunResult>, ApiError> {
    let instance = parse_id(&id)?;
    let service = instance_service(&state.cluster.state().await, &instance)?;
    let action = service
        .x_clouduler
        .and_then(|x| x.actions.get(&name).filter(|a| a.show_in_ui).cloned())
        .ok_or_else(|| ApiError::NotFound(format!("no action {name:?} on {}", instance.service)))?;
    Ok(Json(
        exec_run(&state, &instance, action_request(&action)).await?,
    ))
}

fn action_request(action: &cl_config::ServiceAction) -> ExecRunRequest {
    ExecRunRequest {
        cmd: action.argv(),
        user: action.user.clone(),
        stdin: None,
        timeout_secs: action.timeout.map_or(30, |d| d.as_secs()),
        max_output: default_exec_output(),
        standalone: action.standalone,
    }
}

/// For callers outside the api (replication hooks). `Err` is a one-line reason.
pub async fn run_action_local(
    docker: &Docker,
    instance: &InstanceId,
    action: &cl_config::ServiceAction,
) -> Result<(), String> {
    if action.standalone {
        return Err("a standalone action cannot run as a hook".to_string());
    }
    let r = exec_run_docker(docker, instance, action_request(action))
        .await
        .map_err(|e| e.to_string())?;
    if r.timed_out {
        return Err("timed out".to_string());
    }
    match r.exit_code {
        Some(0) => Ok(()),
        code => Err(format!(
            "exit {}: {}",
            code.map_or("?".to_string(), |c| c.to_string()),
            r.stderr.trim()
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FileRead {
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) truncated: bool,
    pub(crate) is_binary: bool,
    pub(crate) content: Option<String>,
    pub(crate) kind: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) entries: Vec<String>,
}

/// Binary means a NUL or invalid utf-8 in the first 8 KiB; a multi-byte char merely cut by that
/// window (or the caller's size cap) is not evidence of binary.
fn looks_binary(data: &[u8]) -> bool {
    let head = &data[..data.len().min(BINARY_SNIFF_BYTES)];
    if head.contains(&0) {
        return true;
    }
    match std::str::from_utf8(head) {
        Ok(_) => false,
        // `error_len() == None` is only innocent when the sniff window really cut the data short.
        Err(e) => !(e.error_len().is_none() && data.len() > head.len()),
    }
}

fn file_read_from(path: &str, read: cl_docker::PathRead) -> FileRead {
    let kind = match read.kind {
        cl_docker::PathKind::File => "file",
        cl_docker::PathKind::Dir => "dir",
        cl_docker::PathKind::Symlink => "symlink",
        cl_docker::PathKind::Other => "other",
    };
    let (is_binary, content) = if read.kind == cl_docker::PathKind::File {
        if looks_binary(&read.data) {
            (true, None)
        } else {
            (
                false,
                Some(String::from_utf8_lossy(&read.data).into_owned()),
            )
        }
    } else {
        (false, None)
    };
    FileRead {
        path: path.to_string(),
        size: read.size,
        truncated: read.truncated,
        is_binary,
        content,
        kind: kind.to_string(),
        entries: read.entries,
    }
}

/// Uses docker's archive endpoint rather than `exec cat`, so it works in shell-less images.
pub(crate) async fn read_file_local(
    state: &AppState,
    instance: &InstanceId,
    path: &str,
    max: usize,
) -> Result<FileRead, ApiError> {
    if !path.starts_with('/') || path.contains('\0') {
        return Err(ApiError::BadRequest(
            "path must be absolute and must not contain NUL".to_string(),
        ));
    }
    let max = max.clamp(1, MAX_BUFFERED_BYTES);
    let docker = state.docker().await?;
    let read = docker
        .read_path(&instance.container_name(), path, max, FILE_LIST_LIMIT)
        .await
        .map_err(docker_error)?;
    Ok(file_read_from(path, read))
}

#[derive(Deserialize, Serialize)]
struct FileQuery {
    path: String,
    max: Option<usize>,
}

fn internal_file_path(instance: &InstanceId, query: &FileQuery) -> String {
    let qs = serde_urlencoded::to_string(query).unwrap_or_default();
    format!("/api/instances/internal/{instance}/file?{qs}")
}

/// `max` defaults to 256 KiB, capped at 1 MiB.
pub(crate) async fn read_file(
    state: &AppState,
    instance: &InstanceId,
    path: &str,
    max: Option<usize>,
) -> Result<FileRead, ApiError> {
    let node = owning_node(state, instance).await?;
    let max = max.unwrap_or(FILE_READ_DEFAULT).min(MAX_BUFFERED_BYTES);
    if node == state.node_name {
        return read_file_local(state, instance, path, max).await;
    }
    let query = FileQuery {
        path: path.to_string(),
        max: Some(max),
    };
    internal_peer::fetch_json(state, &node, &internal_file_path(instance, &query)).await
}

async fn internal_file(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<FileQuery>,
) -> Result<Json<FileRead>, ApiError> {
    let max = query.max.unwrap_or(FILE_READ_DEFAULT);
    Ok(Json(
        read_file_local(&state, &parse_id(&id)?, &query.path, max).await?,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LogLine {
    pub(crate) stream: String,
    pub(crate) line: String,
    pub(crate) timestamp: Option<String>,
}

/// Parses a `follow=false` SSE log response (`stream::container_log_events`). Keep-alives and unknown
/// events are skipped; an `error` event fails the parse, since a silently shortened tail would mislead.
fn parse_sse_logs(body: &str) -> Result<Vec<LogLine>, String> {
    let body = body.replace("\r\n", "\n");
    let mut lines = Vec::new();
    for block in body.split("\n\n") {
        let mut event = "message";
        let mut data: Vec<&str> = Vec::new();
        for field in block.lines() {
            if let Some(v) = field.strip_prefix("event:") {
                event = v.trim();
            } else if let Some(v) = field.strip_prefix("data:") {
                data.push(v.strip_prefix(' ').unwrap_or(v));
            }
        }
        if data.is_empty() {
            continue;
        }
        let data = data.join("\n");
        match event {
            "stdout" | "stderr" => {
                #[derive(Deserialize)]
                struct Payload {
                    timestamp: Option<String>,
                    text: String,
                }
                let (timestamp, text) = match serde_json::from_str::<Payload>(&data) {
                    Ok(p) => (p.timestamp, p.text),
                    Err(_) => (None, data),
                };
                lines.push(LogLine {
                    stream: event.to_string(),
                    line: text.trim_end_matches(['\n', '\r']).to_string(),
                    timestamp,
                });
            }
            "error" => return Err(data),
            _ => {}
        }
    }
    Ok(lines)
}

/// Local instances read docker; remote ones use the owner's `internal_logs` with `follow=false`,
/// parsing the SSE body back into lines.
pub(crate) async fn fetch_logs(
    state: &AppState,
    instance: &InstanceId,
    tail: u32,
    since: Option<i64>,
) -> Result<Vec<LogLine>, ApiError> {
    let tail = tail.min(LOG_TAIL_LIMIT);
    let node = owning_node(state, instance).await?;
    if node != state.node_name {
        let qs = serde_urlencoded::to_string(LogsQuery {
            follow: false,
            tail: Some(tail),
            since,
        })
        .unwrap_or_default();
        let path = format!("{}?{qs}", internal_logs_path(instance));
        let body = internal_peer::fetch_text(state, &node, &path).await?;
        return parse_sse_logs(&body).map_err(ApiError::Internal);
    }
    let docker = state.docker().await?;
    let opts = LogOptions {
        follow: false,
        tail: Tail::Lines(tail),
        since,
    };
    let name = instance.container_name();
    let stream = docker.logs(&name, &opts);
    futures::pin_mut!(stream);
    let mut lines = Vec::new();
    while let Some(item) = stream.next().await {
        let line = item.map_err(docker_error)?;
        lines.push(LogLine {
            stream: match line.stream {
                cl_docker::LogStream::Stdout => "stdout",
                cl_docker::LogStream::Stderr => "stderr",
            }
            .to_string(),
            line: line.text.trim_end_matches(['\n', '\r']).to_string(),
            timestamp: line.timestamp,
        });
    }
    if let Some(line) = docker.start_failure_line(&name).await {
        lines.push(LogLine {
            stream: "stderr".to_string(),
            line: line.text,
            timestamp: None,
        });
    }
    Ok(lines)
}

#[cfg(test)]
mod inspect_tests {
    use super::*;

    #[test]
    fn sse_logs_parse_both_streams_with_timestamps() {
        let body = "event: stdout\ndata: {\"timestamp\":\"2026-01-01T00:00:00Z\",\"text\":\"hello\\n\"}\n\n\
                    event: stderr\ndata: {\"timestamp\":null,\"text\":\"oops\"}\n\n";
        let lines = parse_sse_logs(body).unwrap();
        assert_eq!(
            lines,
            vec![
                LogLine {
                    stream: "stdout".into(),
                    line: "hello".into(),
                    timestamp: Some("2026-01-01T00:00:00Z".into()),
                },
                LogLine {
                    stream: "stderr".into(),
                    line: "oops".into(),
                    timestamp: None,
                },
            ]
        );
    }

    #[test]
    fn sse_logs_skip_keepalives_and_accept_crlf() {
        let body =
            ": keep-alive\r\n\r\nevent: stdout\r\ndata: {\"timestamp\":null,\"text\":\"a\"}\r\n\r\n";
        let lines = parse_sse_logs(body).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line, "a");
    }

    #[test]
    fn sse_logs_error_event_fails_the_parse() {
        let body = "event: stdout\ndata: {\"timestamp\":null,\"text\":\"a\"}\n\nevent: error\ndata: docker went away\n\n";
        assert_eq!(parse_sse_logs(body).unwrap_err(), "docker went away");
    }

    #[test]
    fn sse_logs_empty_body_is_no_lines() {
        assert!(parse_sse_logs("").unwrap().is_empty());
    }

    #[test]
    fn nul_means_binary() {
        assert!(looks_binary(b"abc\0def"));
    }

    #[test]
    fn plain_and_multibyte_text_is_not_binary() {
        assert!(!looks_binary("héllo wörld ✓".as_bytes()));
        assert!(!looks_binary(b""));
    }

    #[test]
    fn invalid_utf8_means_binary() {
        assert!(looks_binary(&[0x66, 0xff, 0xfe, 0x6f]));
    }

    #[test]
    fn a_character_cut_by_the_sniff_window_is_not_binary() {
        // 'é' is two bytes; put its first byte on the last sniffed byte.
        let mut data = vec![b'a'; BINARY_SNIFF_BYTES - 1];
        data.extend_from_slice("é".as_bytes());
        data.extend_from_slice(b"tail");
        assert!(!looks_binary(&data));
    }

    #[test]
    fn a_character_cut_at_the_very_end_of_the_data_is_binary() {
        // Nothing follows, so the window did not cut it; the data is just
        // malformed.
        assert!(looks_binary(&[b'a', 0xc3]));
    }

    #[test]
    fn capped_buf_keeps_the_prefix_and_flags_the_rest() {
        let mut buf = CappedBuf::new(5);
        buf.push(b"abc");
        assert!(!buf.truncated);
        buf.push(b"defgh");
        assert!(buf.truncated);
        let (text, truncated) = buf.into_text();
        assert_eq!(text, "abcde");
        assert!(truncated);
    }

    #[test]
    fn capped_buf_exactly_at_the_cap_is_not_truncated() {
        let mut buf = CappedBuf::new(3);
        buf.push(b"abc");
        assert!(!buf.truncated);
        buf.push(b"");
        assert!(!buf.truncated);
    }

    #[test]
    fn file_read_reports_a_directory_without_content() {
        let read = cl_docker::PathRead {
            kind: cl_docker::PathKind::Dir,
            size: 4096,
            data: Vec::new(),
            truncated: false,
            entries: vec!["a".into(), "b".into()],
        };
        let out = file_read_from("/etc", read);
        assert_eq!(out.kind, "dir");
        assert!(out.content.is_none() && !out.is_binary);
        assert_eq!(out.entries, vec!["a", "b"]);
    }

    #[test]
    fn file_read_withholds_binary_content() {
        let read = cl_docker::PathRead {
            kind: cl_docker::PathKind::File,
            size: 3,
            data: vec![0, 1, 2],
            truncated: false,
            entries: Vec::new(),
        };
        let out = file_read_from("/bin/x", read);
        assert!(out.is_binary && out.content.is_none());
    }
}
