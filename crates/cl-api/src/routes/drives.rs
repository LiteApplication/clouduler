//! `/api/drives`, `/api/volumes`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_raft::ids::VolumeId;
use cl_raft::Request as ClusterRequest;
use serde::{Deserialize, Serialize};

use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/drives", get(list_drives))
        .route("/api/drives/{drive}/failback", post(request_failback))
        .route("/api/drives/{drive}/resolve", post(resolve_conflict))
        .route("/api/volumes", get(list_volumes))
        .route("/api/volumes/{drive}/{volume}/sync", post(sync_volume))
        .route("/api/volumes/{drive}/{volume}/inspect", get(inspect_volume))
        .route("/api/volumes/{drive}/{volume}/freeze", post(freeze_volume))
        .route(
            "/api/volumes/{drive}/{volume}/unfreeze",
            post(unfreeze_volume),
        )
        .route(
            "/api/volumes/{drive}/{volume}",
            axum::routing::delete(delete_volume),
        )
}

#[derive(Serialize)]
struct DriveSummary {
    name: String,
    mode: cl_config::DriveMode,
    served_by: Vec<String>,
    /// The first reachable of `served_by`; `None` for a replicated drive or when none is reachable.
    active_server: Option<String>,
    sync_interval: String,
    failback: cl_config::Failback,
    /// `None` for a replicated drive and a served drive that never failed over.
    served: Option<ServedSummary>,
    nodes: Vec<DriveNodeStatus>,
}

#[derive(Serialize)]
struct ServedSummary {
    holder: String,
    epoch: u64,
    /// Nobody serves the drive until the handover ends.
    handover_to: Option<String>,
    /// Both copies were written while apart; replication is stopped until an operator resolves it.
    conflict: Option<ServedConflictSummary>,
    failback_blocked: Option<String>,
}

#[derive(Serialize)]
struct ServedConflictSummary {
    holder: String,
    other: String,
    reason: String,
}

#[derive(Serialize)]
struct DriveNodeStatus {
    node: String,
    reachable: bool,
}

async fn list_drives(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Vec<DriveSummary>> {
    let cluster_config = state.cluster_config.read().await;
    let cluster_state = state.cluster.snapshot().await;
    Json(
        cluster_config
            .drives
            .iter()
            .map(|(name, drive)| DriveSummary {
                name: name.clone(),
                mode: drive.mode,
                served_by: drive.served_by.clone(),
                active_server: cl_raft::served::choose_active_server(
                    &drive.served_by,
                    &cluster_state.unreachable,
                    cluster_state.served.get(name),
                )
                .map(str::to_string),
                sync_interval: humantime::format_duration(drive.sync_interval).to_string(),
                failback: drive.failback,
                served: cluster_state.served.get(name).map(|a| ServedSummary {
                    holder: a.holder.clone(),
                    epoch: a.epoch,
                    handover_to: a.handover.as_ref().map(|h| h.to.clone()),
                    conflict: a.conflict.as_ref().map(|c| ServedConflictSummary {
                        holder: c.holder.clone(),
                        other: c.other.clone(),
                        reason: c.reason.clone(),
                    }),
                    failback_blocked: a
                        .blocked
                        .as_ref()
                        .map(|b| format!("{}: {}", b.node, b.reason)),
                }),
                nodes: cluster_state
                    .nodes
                    .values()
                    .filter(|n| n.implements(name))
                    .map(|n| DriveNodeStatus {
                        node: n.name.clone(),
                        reachable: !cluster_state.unreachable.contains(&n.name),
                    })
                    .collect(),
            })
            .collect(),
    )
}

#[derive(Serialize)]
struct VolumeSummary {
    id: String,
    generation: u64,
    primary: String,
    replicas: Vec<ReplicaStatus>,
    looks_empty: bool,
    frozen: Option<FrozenSummary>,
    sync_refused: Option<cl_raft::state::SyncRefusal>,
}

#[derive(Serialize)]
struct FrozenSummary {
    node: String,
    stopped: Vec<String>,
}

#[derive(Serialize)]
struct ReplicaStatus {
    node: String,
    generation: u64,
    in_sync: bool,
}

async fn list_volumes(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Vec<VolumeSummary>> {
    let cluster_state = state.cluster.snapshot().await;
    Json(
        cluster_state
            .volumes
            .values()
            .map(|v| VolumeSummary {
                id: v.id.to_string(),
                generation: v.generation,
                primary: v.primary.clone(),
                replicas: v
                    .replicas
                    .iter()
                    .map(|(node, gen)| ReplicaStatus {
                        node: node.clone(),
                        generation: *gen,
                        in_sync: *gen == v.generation,
                    })
                    .collect(),
                looks_empty: v.looks_empty,
                frozen: v.frozen.as_ref().map(|f| FrozenSummary {
                    node: f.node.clone(),
                    stopped: f.stopped.iter().map(|i| i.to_string()).collect(),
                }),
                sync_refused: v.sync_refused.clone(),
            })
            .collect(),
    )
}

async fn submit_served(
    state: &AppState,
    drive: String,
    op: cl_raft::served::ServedOp,
) -> Result<StatusCode, ApiError> {
    let response = state
        .cluster
        .submit(ClusterRequest::Served { drive, op })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::OK)
}

/// The holder still checks that the preferred server's copy can safely take over, and copies anything missing first.
async fn request_failback(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(drive): Path<String>,
) -> Result<StatusCode, ApiError> {
    if !state
        .cluster_config
        .read()
        .await
        .drives
        .get(&drive)
        .is_some_and(|d| d.is_served())
    {
        return Err(ApiError::NotFound(format!("no served drive {drive}")));
    }
    submit_served(&state, drive, cl_raft::served::ServedOp::RequestFailback).await
}

#[derive(Deserialize)]
struct ResolveBody {
    keep: String,
}

async fn resolve_conflict(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(drive): Path<String>,
    Json(body): Json<ResolveBody>,
) -> Result<StatusCode, ApiError> {
    submit_served(
        &state,
        drive,
        cl_raft::served::ServedOp::Resolve { keep: body.keep },
    )
    .await
}

/// Not a raft command: `cl-drive` owns the sync loop. `TouchVolume` bumps the generation, which its next pass watches for.
async fn sync_volume(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path((drive, volume)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let id = VolumeId::new(drive, volume);
    let cluster_state = state.cluster.snapshot().await;
    let current = cluster_state
        .volumes
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("no volume {id}")))?;

    let response = state
        .cluster
        .submit(ClusterRequest::TouchVolume {
            id,
            primary: current.primary.clone(),
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::OK)
}

#[derive(Serialize)]
struct VolumeInspection {
    node: String,
    files: usize,
    bytes: u64,
    newest_file_at: Option<i64>,
}

#[derive(Deserialize)]
struct InspectQuery {
    node: Option<String>,
}

/// Read from the source node on demand (the cluster only records "looks empty"); `?node=` asks a replica to compare copies.
async fn inspect_volume(
    _identity: Identity,
    State(state): State<AppState>,
    Path((drive, volume)): Path<(String, String)>,
    Query(query): Query<InspectQuery>,
) -> Result<Json<VolumeInspection>, ApiError> {
    let id = VolumeId::new(drive, volume);
    let cluster_state = state.cluster.snapshot().await;
    let record = cluster_state
        .volumes
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("no volume {id}")))?;
    let served_by = state
        .cluster_config
        .read()
        .await
        .drives
        .get(&id.drive)
        .filter(|d| d.is_served())
        .map(|d| d.served_by.clone());
    let source = match (query.node, served_by) {
        (Some(node), _) => {
            if node != record.primary && !record.replicas.contains_key(&node) {
                return Err(ApiError::NotFound(format!("{node} holds no copy of {id}")));
            }
            node
        }
        (None, Some(candidates)) => candidates
            .into_iter()
            .find(|n| !cluster_state.unreachable.contains(n))
            .ok_or_else(|| ApiError::Conflict(format!("no node is serving {}", id.drive)))?,
        (None, None) => record.primary.clone(),
    };
    let addr = cluster_state
        .nodes
        .get(&source)
        .map(|n| n.rpc_addr)
        .ok_or_else(|| ApiError::NotFound(format!("node {source} is not in the cluster")))?;
    let key = state
        .key
        .read()
        .await
        .clone()
        .ok_or_else(|| ApiError::Conflict("this node does not hold the cluster key".into()))?;
    let peer = std::net::SocketAddr::new(addr.ip(), cl_drive::drive_port(addr.port()));
    let manifest =
        cl_drive::transport::request_manifest(peer, &id, false, &cl_raft::fixed_key(key))
            .await
            .map_err(|e| ApiError::Conflict(format!("asking {source} about {id}: {e}")))?;
    let regular = || manifest.files.values().filter(|f| !f.is_symlink);
    Ok(Json(VolumeInspection {
        node: source,
        files: manifest.files.len(),
        bytes: regular().map(|f| f.size).sum(),
        newest_file_at: regular()
            .map(|f| f.mtime_nanos.div_euclid(1_000_000_000))
            .max(),
    }))
}

async fn delete_volume(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path((drive, volume)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let id = VolumeId::new(drive, volume);
    let response = state
        .cluster
        .submit(ClusterRequest::RemoveVolume { id })
        .await
        .map_err(ApiError::not_leader)?;
    match response.error {
        Some(msg) => Err(ApiError::Conflict(msg)),
        None => Ok(StatusCode::NO_CONTENT),
    }
}

#[derive(Deserialize, Default)]
struct FreezeRequest {
    node: Option<String>,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize, Default)]
struct UnfreezeRequest {
    start_containers: Option<bool>,
}

/// Stopping containers on other nodes is requested but not waited for; the volume reads as frozen straight away.
async fn freeze_volume(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path((drive, volume)): Path<(String, String)>,
    body: Option<Json<FreezeRequest>>,
) -> Result<StatusCode, ApiError> {
    let id = VolumeId::new(drive, volume);
    let Json(body) = body.unwrap_or_default();
    crate::volume_ops::freeze(&state, &id, body.node, body.force, None)
        .await
        .map_err(ApiError::Conflict)?;
    Ok(StatusCode::ACCEPTED)
}

/// Returns at once; waiting for every node to receive the edited copy and restarting containers continues in the background.
async fn unfreeze_volume(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path((drive, volume)): Path<(String, String)>,
    body: Option<Json<UnfreezeRequest>>,
) -> Result<StatusCode, ApiError> {
    let id = VolumeId::new(drive, volume);
    let Json(body) = body.unwrap_or_default();
    let frozen = crate::volume_ops::begin_unfreeze(&state, &id)
        .await
        .map_err(ApiError::Conflict)?;
    let start = body.start_containers.unwrap_or(true);
    tokio::spawn(async move {
        let wait = crate::volume_ops::wait_of(None);
        match crate::volume_ops::finish_unfreeze(&state, &id, &frozen, start, wait).await {
            Ok(msg) => tracing::info!("{msg}"),
            Err(e) => tracing::warn!(volume = %id, error = %e, "finishing the unfreeze failed"),
        }
    });
    Ok(StatusCode::ACCEPTED)
}
