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
        .route("/api/instances/{id}/move", post(move_instance))
        .route("/api/rebalance", post(rebalance))
        .route("/api/instances/{id}/update", post(update))
        .route("/api/instances/{id}/logs", get(logs))
        .route("/api/instances/{id}/stats", get(stats))
        .route("/api/instances/events", get(instance_events))
        // Node-to-node only, HMAC authenticated: what the public routes above
        // forward to when the instance they were asked about lives on another
        // node. See `internal_peer`.
        .route(
            "/api/instances/internal/{id}/summary",
            get(internal_summary),
        )
        .route("/api/instances/internal/{id}/logs", get(internal_logs))
        .route("/api/instances/internal/{id}/stats", get(internal_stats))
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
        // Node-to-node only, same as above: the cli resolves which node an
        // instance runs on itself (see `clouduler exec`) and connects to
        // that node directly, so there is no public forwarding counterpart
        // to this one yet.
        .route("/api/instances/internal/{id}/exec", get(internal_exec))
        // Node-to-node only, same as `exec`: `clouduler capture` resolves the
        // instance's node itself and connects here directly.
        .route(
            "/api/instances/internal/{id}/capture",
            get(internal_capture),
        )
        // Node-to-node only: the buffered counterparts of `exec`/`logs` that
        // the MCP layer calls through `exec_run` and `read_file`.
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
    /// What the cluster wants: "running" or "stopped".
    pub(crate) desired: String,
    /// What the container is actually doing.
    pub(crate) state: String,
    pub(crate) running: Option<bool>,
    pub(crate) health: String,
    pub(crate) image: Option<String>,
    pub(crate) image_digest: Option<String>,
    /// Registry content digest of the bits the container is *actually*
    /// running, as opposed to `image_digest`, which is what the cluster has
    /// pinned. The two differ whenever a pin was recorded but the container
    /// has not been rebuilt onto it yet. `None` when the instance runs
    /// elsewhere, has no container, or runs a locally built image that was
    /// never pulled from a registry and so has no digest at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) running_digest: Option<String>,
    pub(crate) started_at: Option<String>,
    pub(crate) migration: Option<MigrationSummary>,
    /// What is actively happening to this instance right now — a redeploy
    /// sub-step or a migration phase — or `None` when it is steady-state.
    /// `state`/`migration` above are the raw facts; this is the same
    /// information composed into one string plus a `(step, total)` pair for
    /// a progress bar. See `compose_progress`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) step: Option<(u8, u8)>,
    /// Why this instance has no assignment at all, set only when `state` is
    /// `"pending_placement"`: no node in the cluster currently satisfies
    /// what its service needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) placement_error: Option<String>,
    /// The reverse proxy's own most recent active health check of this
    /// instance's service, when it publishes a route at all. `None` means
    /// either nothing is published for it or no check has landed yet, not
    /// that it's healthy — distinct from an empty `error` on a `Some`, which
    /// does mean healthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) proxy: Option<ProxyHealthSummary>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ProxyHealthSummary {
    pub(crate) host: String,
    pub(crate) checked_at: String,
    /// `None` when the last check reached the backend; `Some(reason)` in the
    /// same shape the proxy's own "bad gateway" response would have shown a
    /// real client, e.g. `"10.244.1.2:8080 unreachable: client error (SendRequest)"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct MigrationSummary {
    pub(crate) to: String,
    pub(crate) phase: String,
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

/// The actual docker inspection, valid only on the node running the
/// container. Also the graceful "unknown" answer for a remote assignment,
/// used both as `summarize`'s fallback and as the direct answer for an
/// instance genuinely assigned elsewhere (docker down, container not up yet).
async fn summarize_local(
    state: &AppState,
    cluster_state: &cl_raft::ClusterState,
    assignment: &cl_raft::Assignment,
) -> InstanceSummary {
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
    // Looked up by the local image id the container was created from, not by
    // its tag: the tag has usually moved on, which is exactly the drift this
    // is here to expose.
    let running_digest = match (&container, state.docker.read().await.clone()) {
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
    let instance_state = if !local {
        STATE_UNKNOWN
    } else {
        match &container {
            Some(c) => run_state_to_summary(c.status, assignment.desired),
            // Assigned here but no container yet: either the image is still
            // being fetched, or it is about to be created.
            None if state
                .pulling
                .read()
                .await
                .contains_key(&assignment.instance) =>
            {
                STATE_PULLING
            }
            None => STATE_STOPPED,
        }
    };

    let migration_sync = assignment.migration.as_ref().and_then(|_| {
        state
            .migration_sync
            .read()
            .unwrap()
            .get(&assignment.instance)
            .copied()
    });
    // Replicated, so this is the real answer even for an instance whose node
    // could not be asked just now.
    let (reason, step) = compose_progress(
        cluster_state.instance_updates.get(&assignment.instance),
        assignment
            .migration
            .as_ref()
            .map(|m| (&m.phase, migration_sync)),
    );

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
            phase: format!("{:?}", m.phase).to_lowercase(),
        }),
        placement_error: None,
        proxy,
        reason,
        step,
    }
}

/// This node's own reverse proxy's latest check of the service this instance
/// belongs to, keyed `stack.service` since one route load balances every
/// replica of a service rather than publishing one per instance. `None` when
/// the service publishes no route, or no check has run for it yet.
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

/// One instance's update step, or migration phase, turned into the string
/// the ui shows verbatim and a `(step, total)` pair for anything that wants
/// to render a progress bar instead. `None` for a steady-state instance.
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

/// Pulling, stopping, starting, started.
const UPDATE_TOTAL_STEPS: u8 = 4;

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
    let label = format!("{phase:?}").to_lowercase();
    (Some(format!("migrating ({label})")), None)
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

/// Every instance's current `(state, reason, step)`, for `/api/instances/events`
/// to diff against what it last sent. Reuses `summarize`'s existing
/// local/remote split — a remote instance still gets a real answer via the
/// same internal forwarding `list` already relies on.
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

/// Every instance with a live summary, plus the ones no node can currently
/// host. What `GET /api/instances` returns, callable in-process (the MCP
/// layer) without going through axum.
pub(crate) async fn list_all(state: &AppState) -> Vec<InstanceSummary> {
    let cluster_state = state.cluster.state().await;
    // Concurrent rather than one at a time: a single unreachable node whose
    // instances sit behind `FETCH_TIMEOUT` must not serialize onto the whole
    // list's latency.
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

/// Instances every deployed stack wants but that have no assignment at all,
/// because no node in the cluster currently satisfies what they need. Nothing
/// in `cluster_state.assignments` represents these, so they are computed live
/// from the same placement logic the scheduler runs, rather than stored.
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
            running_digest: None,
            started_at: None,
            migration: None,
            reason: None,
            step: None,
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

    Ok(Json(summarize(&state, &cluster_state, assignment).await))
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
    Ok(Json(
        summarize_local(&state, &cluster_state, assignment).await,
    ))
}

/// Rerolls the instance's placement rather than restarting it in place: works
/// out where it would land if freshly scheduled right now (the same node, if
/// that node is still the best fit, or another one if it no longer is -
/// unreachable, drained, missing a needed drive or network) and moves it
/// there before starting it. An in-place restart would instead retry forever
/// on whatever node it already happened to be on, even one that can no
/// longer host it.
///
/// Reroll happens inline here rather than by unassigning and waiting for the
/// scheduler's own sweep, for two reasons: the operator sees the effect
/// immediately instead of up to `SWEEP_INTERVAL` later, and a plain
/// `Unassign` would drop the assignment's `image_digest` pin (`Assign`
/// always resets it to `None`) - restarting a `stack update`-pinned instance
/// would silently let it drift back to whatever tag it deploys from now
/// resolves to.
async fn restart(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    restart_instance(&state, parse_id(&id)?).await?;
    Ok(StatusCode::OK)
}

/// The body of `restart`, callable in-process. See there for why this
/// rerolls placement rather than restarting in place.
pub(crate) async fn restart_instance(
    state: &AppState,
    instance: InstanceId,
) -> Result<(), ApiError> {
    let id = instance.to_string();
    let cluster_state = state.cluster.state().await;
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
    let nat_networks = config.nat_networks();
    drop(config);

    let item = desired
        .into_iter()
        .find(|d| d.id == instance)
        .ok_or_else(|| ApiError::NotFound(format!("no instance {id}")))?;

    // Placement leaves a valid existing assignment alone, so judge candidates
    // against a copy with this one instance's assignment cleared - the same
    // fresh pick a never-yet-placed instance would get.
    let mut probe = cluster_state.clone();
    probe.assignments.remove(&instance);
    let outcome = cl_sched::place::place(
        &probe,
        std::slice::from_ref(&item),
        &served_drives,
        &nat_networks,
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
        .and_then(|file| {
            file.services
                .get(&instance.service)
                .map(Service::drives_used)
        })
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
async fn rebalance(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> Result<Json<RebalanceResponse>, ApiError> {
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

    let planned =
        cl_sched::rebalance::plan(&cluster_state, &desired, &served_drives, &nat_networks);

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
    update_instance_guarded(&state, parse_id(&id)?).await?;
    Ok(StatusCode::OK)
}

/// `update_instance`, refusing with a conflict when an update of the same
/// instance is already running. What `POST /api/instances/{id}/update` does,
/// callable in-process.
pub(crate) async fn update_instance_guarded(
    state: &AppState,
    instance: InstanceId,
) -> Result<(), ApiError> {
    // A second update on top of one in progress would pull again and remove
    // the container the first one is waiting on. The bulk path
    // (`updates::apply_selection`) queues its own record first, so it does
    // not come through here.
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

/// Re-pins `digest` for `instance` and removes its container so reconcile
/// rebuilds it on that digest: how a failed rolling update undoes itself.
/// Wherever the instance runs.
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
    if node != state.node_name {
        let path = format!("/api/instances/internal/{instance}/remove-container");
        return internal_peer::post_empty(state, &node, &path).await;
    }
    remove_container(state, instance).await
}

async fn remove_container(state: &AppState, instance: &InstanceId) -> Result<(), ApiError> {
    let name = instance.container_name();
    let docker = state.docker().await?;
    // The reconcile loop's own fallback (`failover.stop_grace`) rather than
    // docker's 10 seconds, which is short for a database.
    let stop_grace = state.cluster_config.read().await.failover.stop_grace;
    docker.stop(&name, stop_grace).await.ok();
    docker
        .remove(&name, true)
        .await
        .map_err(ApiError::internal)?;
    Ok(())
}

/// Node-to-node half of `roll_back_instance`.
async fn internal_remove_container(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    remove_container(&state, &parse_id(&id)?).await?;
    Ok(StatusCode::OK)
}

/// Update one instance wherever it runs: here, or on the node that owns it.
///
/// Shared with `routes::updates`, which drives the same thing across a whole
/// list of instances from the Updates view.
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

/// The first half of an update, wherever the instance runs: downloads its
/// image and returns the registry digest to pin. The old container keeps
/// serving. `replace_instance` is the second half.
///
/// Split so a rolling update can download the next instance on a node while
/// the previous one is restarting.
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

/// The second half of an update: pins `digest` and swaps the container.
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

/// Pulls the instance's current image on this node, records the new digest
/// (a leader-only raft write, hence still needing `submit` even when this is
/// not the leader), and takes the old container away so the reconcile loop
/// builds a new one on the new bits.
///
/// Removing rather than restarting is the whole point. `docker restart` stops
/// and starts *the same container*, which is bound to the image id it was
/// created from — so the pull would fetch the new image, the digest would be
/// recorded, the container would bounce, and it would come back up running
/// exactly the bits it was already running. Nothing else would have caught
/// it either: the spec revision that drives `Action::Recreate` is built from
/// the stack revision and mount shape (`revision_with_mounts`), not the
/// image, so a changed pin alone never triggers a replace. The update looked
/// like it worked and silently did nothing.
///
/// Reconcile sees a wanted instance with no container and issues
/// `Action::Create` on its next sweep, which builds the spec from the pin
/// just recorded. Ordering against that write does not matter: if the sweep
/// lands before the pin has applied locally, it creates from the plain tag,
/// which the pull above has just repointed at the same new digest.
///
/// Every step is recorded in `ClusterState::instance_updates` as it happens;
/// the reconciler records the last one (`Started`) once the new container is
/// up.
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

/// `pull_instance` on this node, recording a failure the way `update_local`
/// does.
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

/// `replace_instance` on this node, recording a failure the way
/// `update_local` does.
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
    let name = instance.container_name();
    let docker = state.docker().await?;
    docker
        .inspect(&name)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            ApiError::NotFound(format!("container {name} does not exist on this node"))
        })?;

    // The stack's declared reference, e.g. `nginx:1.25` — not the running
    // container's `image`, which once this instance has been pinned before
    // is a `repo@sha256:...` reference with the tag discarded. Pulling that
    // is a no-op forever: a digest reference is immutable, so it always
    // "pulls" the exact same bits already running, no matter how many new
    // tags have been pushed since. See `updates::declared_image`.
    let cluster_state = state.cluster.state().await;
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
    // Marked for the same reason the reconcile loop marks its own pulls: a
    // docker inspect cannot tell "fetching the image" from "about to be
    // created", so without this the instance reads as plain `stopped` for
    // however long the download takes. `apply` returns before the pull
    // finishes, so this is the only thing the ui has to watch.
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
    let running = docker
        .inspect(&name)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            ApiError::NotFound(format!("container {name} does not exist on this node"))
        })?;

    // Remember what is running now, so a failed update can put it back. An
    // already-pinned instance has it recorded as the pin; an unpinned one
    // does not, and the pull has just moved its tag, so resolve the
    // running image's registry digest (by local image id, which the pull did
    // not touch) and pin that first. The pin the next request writes then
    // becomes the "previous" one.
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
    // The reconcile loop's own fallback (`failover.stop_grace`) rather than
    // docker's 10 seconds, which is short for a database.
    let stop_grace = state.cluster_config.read().await.failover.stop_grace;
    docker.stop(&name, stop_grace).await.ok();
    docker
        .remove(&name, true)
        .await
        .map_err(ApiError::internal)?;
    progress.report(instance, UpdatePhase::Starting, None).await;
    Ok(())
}

/// Node-to-node: downloads this node's own instance's image.
async fn internal_pull(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Pulled>, ApiError> {
    let digest = pull_local(&state, &parse_id(&id)?).await?;
    Ok(Json(Pulled { digest }))
}

/// Node-to-node: pins the pulled digest and swaps this node's own container.
async fn internal_replace(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(pulled): Query<Pulled>,
) -> Result<StatusCode, ApiError> {
    replace_local(&state, &parse_id(&id)?, pulled.digest).await?;
    Ok(StatusCode::OK)
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
            let _ = socket
                .send(Message::Text(format!("exec: {e}").into()))
                .await;
            return;
        }
    };

    // The client's own stdin - not just its tty state - drives when it sends
    // `Close`: a non-interactive caller (piped stdin, e.g. `ssh host clouduler
    // exec ...` without `-tt`) hits EOF on its stdin almost immediately and
    // closes right away, often before the command has produced any output at
    // all. That close only means "no more input" - closing `session.input`
    // (which the docker exec sees as its stdin closing, same as a real
    // terminal hanging up) is the correct reaction, not tearing down the
    // whole session and discarding output the command hasn't gotten to write
    // yet.
    let exec_id = session.id.clone();
    let mut input = Some(session.input);
    let mut finished = false;
    loop {
        tokio::select! {
            incoming = socket.recv(), if input.is_some() => {
                match incoming {
                    // An empty frame is the client's stdin EOF (see `cl-cli`'s
                    // `exec::bridge`): half-close the container's stdin, but
                    // keep relaying its output.
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
                    // The command exited, or docker itself errored: either
                    // way there is nothing left to relay.
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
    // Tell the client how the command ended, so it can exit with the same
    // code (`exit:<n>`; every other text frame is an error message).
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
    /// Which of the stack's declared networks to capture. Defaults to the
    /// same network `clouduler stack deploy` builds a VXLAN overlay for (see
    /// `cl_sched::run::overlay_network_name`) - the one the proxy actually
    /// forwards to, which is what someone chasing a proxy bug wants to see
    /// without having to know the stack's network layout first.
    network: Option<String>,
    /// A raw BPF filter expression (`tcpdump`'s own syntax), space-split on
    /// this end since it arrives as a single query value. Good enough for
    /// the common case (`port 2283`, `host 10.244.7.11 and port 2283`); a
    /// filter that itself needs quoting is out of scope here.
    filter: Option<String>,
    /// Stream raw pcap bytes (`tcpdump -w -`) instead of decoded text lines,
    /// so the cli can save them straight to a `.pcap` file for Wireshark.
    #[serde(default)]
    pcap: bool,
}

/// Node-to-node only: runs `tcpdump` on this node's own bridge for one of a
/// stack's overlay networks and streams its output back over a websocket,
/// the same shape as `internal_exec`. This is what used to mean SSHing into
/// a cluster node and reasoning out its bridge interface name by hand (see
/// `cl_net::vxlan::bridge_name`) before running `tcpdump` there as root -
/// this endpoint does exactly that, from the daemon's own root privileges,
/// so `clouduler capture` can do it from a laptop instead.
async fn internal_capture(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<CaptureQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let instance = parse_id(&id)?;
    let cluster_state = state.cluster.state().await;
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

    // Relayed on its own task rather than in the main select! below: once
    // stderr hits EOF (immediately, if tcpdump never writes to it) a select!
    // arm would keep firing on that same `Ok(0)` forever and spin the loop.
    // A channel that just stops sending once its task ends needs no such
    // care in the receiver.
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
            // Only polled while the sender task is still alive; once it ends
            // (`None`) the branch is disabled instead of spinning on a
            // permanently-ready `None` forever.
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

// ---------------------------------------------------------------------------
// Buffered inspection for the MCP layer: run a command, read a file, fetch
// logs, each returning plain values rather than a stream or a websocket.
//
// Every `*_local` half talks to this node's docker; the public half runs it
// here or forwards to the owning node over the internal api (HMAC node-to-node
// auth over the cluster key, see `internal_peer`). None of this redacts: what
// comes back is raw container output, and scrubbing secrets out of it is the
// MCP layer's job on the node that serves the MCP request.
// ---------------------------------------------------------------------------

/// Hard ceiling on one buffered command's runtime. The forwarding client's
/// write timeout is 300s too, so a command at the cap still gets an answer.
const EXEC_RUN_MAX_SECS: u64 = 300;
const EXEC_RUN_DEFAULT_SECS: u64 = 30;
/// Hard ceiling on bytes kept per stream (and on stdin accepted), and on a
/// file read. A buffered answer travels as json and ends up in a model's
/// context, so there is no use for more.
const MAX_BUFFERED_BYTES: usize = 1024 * 1024;
const EXEC_RUN_DEFAULT_OUTPUT: usize = 64 * 1024;
const FILE_READ_DEFAULT: usize = 256 * 1024;
/// Directory listings stop here; the rest is reported as `truncated`.
const FILE_LIST_LIMIT: usize = 500;
/// How much of a file is inspected when deciding text vs binary.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// Most log lines one buffered fetch returns.
const LOG_TAIL_LIMIT: u32 = 10_000;

fn default_exec_timeout() -> u64 {
    EXEC_RUN_DEFAULT_SECS
}

fn default_exec_output() -> usize {
    EXEC_RUN_DEFAULT_OUTPUT
}

/// Run a command in an instance's container and wait for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecRunRequest {
    pub(crate) cmd: Vec<String>,
    #[serde(default)]
    pub(crate) user: Option<String>,
    /// Written to the command's stdin, which is then closed. Without it stdin
    /// is closed immediately, so a command that reads it sees EOF.
    #[serde(default)]
    pub(crate) stdin: Option<String>,
    /// Clamped to 1..=300.
    #[serde(default = "default_exec_timeout")]
    pub(crate) timeout_secs: u64,
    /// Per stream, clamped to 1..=1 MiB.
    #[serde(default = "default_exec_output")]
    pub(crate) max_output: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecRunResult {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    /// `None` when the command timed out, or docker had not recorded a code.
    pub(crate) exit_code: Option<i64>,
    pub(crate) timed_out: bool,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
}

/// Bytes kept up to a cap, remembering whether anything was dropped. Output
/// past the cap is still read (and discarded) so the command is not blocked
/// writing to a full pipe while it finishes.
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

/// `exec_run` on this node's docker.
///
/// Not a tty, so stdout and stderr stay separate. On timeout this stops
/// reading and reports `timed_out`, but the command itself keeps running
/// inside the container: the docker api has no way to kill an exec, only to
/// stop listening to it. Callers should not assume a timed-out command did
/// not finish its side effects.
pub(crate) async fn exec_run_local(
    state: &AppState,
    instance: &InstanceId,
    req: ExecRunRequest,
) -> Result<ExecRunResult, ApiError> {
    if req.cmd.is_empty() {
        return Err(ApiError::BadRequest("cmd must not be empty".to_string()));
    }
    let stdin = req.stdin.unwrap_or_default().into_bytes();
    if stdin.len() > MAX_BUFFERED_BYTES {
        return Err(ApiError::BadRequest(format!(
            "stdin is limited to {MAX_BUFFERED_BYTES} bytes"
        )));
    }
    let timeout = Duration::from_secs(req.timeout_secs.clamp(1, EXEC_RUN_MAX_SECS));
    let cap = req.max_output.clamp(1, MAX_BUFFERED_BYTES);

    let docker = state.docker().await?;
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

    // Written from its own task: a command that fills its stdout before it
    // has read all of stdin would otherwise deadlock against this loop.
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
    })
}

fn internal_exec_run_path(instance: &InstanceId) -> String {
    format!("/api/instances/internal/{instance}/exec-run")
}

/// Run a command in an instance's container wherever it runs: here, or on
/// the owning node via `exec-run`. See `exec_run_local` for the timeout
/// caveat.
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

/// Node-to-node: buffered exec in this node's own container. Takes a body, so
/// the signed body hash is checked against what was actually read.
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

/// A file or directory read out of a container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FileRead {
    pub(crate) path: String,
    /// Size of the whole file in bytes, not of what was returned.
    pub(crate) size: u64,
    /// More exists than was returned: file content cut at `max`, or a
    /// directory with more than 500 entries.
    pub(crate) truncated: bool,
    pub(crate) is_binary: bool,
    /// The file's text. `None` for a binary file, and for anything that is
    /// not a regular file.
    pub(crate) content: Option<String>,
    /// "file", "dir", "symlink" or "other".
    pub(crate) kind: String,
    /// A directory's entry names (sorted, immediate children only); empty
    /// otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) entries: Vec<String>,
}

/// Whether `data` looks like binary rather than text: a NUL or invalid utf-8
/// in its first 8 KiB. A multi-byte character merely cut by that 8 KiB
/// boundary (or by the caller's own size cap) is not evidence of binary.
fn looks_binary(data: &[u8]) -> bool {
    let head = &data[..data.len().min(BINARY_SNIFF_BYTES)];
    if head.contains(&0) {
        return true;
    }
    match std::str::from_utf8(head) {
        Ok(_) => false,
        // `error_len() == None` means the input ended mid-character, which is
        // only innocent when the sniff window really did cut the data short.
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

/// `read_file` on this node's docker. Uses docker's archive endpoint rather
/// than `exec cat`, so it works in images with no shell (distroless).
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

/// Read a file (or list a directory) in an instance's container wherever it
/// runs. `max` defaults to 256 KiB and is capped at 1 MiB.
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

/// Node-to-node: read a file from this node's own container.
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

/// One buffered container log line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LogLine {
    /// "stdout" or "stderr".
    pub(crate) stream: String,
    pub(crate) line: String,
    pub(crate) timestamp: Option<String>,
}

/// Parse the body of a `follow=false` SSE log response (see
/// `stream::container_log_events`): `event: stdout|stderr` with a json
/// `{"timestamp", "text"}` payload per line. Keep-alive comments and unknown
/// events are skipped; an `error` event fails the whole parse, since the log
/// was cut short and a silently shortened tail would mislead.
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
                // Falls back to the raw data for the plain-text shape
                // `container_log_events` uses if it cannot encode a line.
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

/// The last `tail` lines (capped at 10,000) of an instance's container log,
/// optionally only after unix time `since`, buffered. Local instances read
/// docker directly; remote ones go through the owning node's existing
/// `internal_logs` route with `follow=false` and the SSE body is parsed back
/// into lines.
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
    let stream = docker.logs(&name, opts);
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
