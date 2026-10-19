//! `/api/drives`, `/api/volumes`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_raft::ids::VolumeId;
use cl_raft::Request as ClusterRequest;
use serde::Serialize;

use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/drives", get(list_drives))
        .route("/api/volumes", get(list_volumes))
        .route("/api/volumes/{drive}/{volume}/sync", post(sync_volume))
        .route("/api/volumes/{drive}/{volume}/inspect", get(inspect_volume))
        .route(
            "/api/volumes/{drive}/{volume}",
            axum::routing::delete(delete_volume),
        )
}

#[derive(Serialize)]
struct DriveSummary {
    name: String,
    mode: cl_config::DriveMode,
    /// Candidate serving nodes for a served drive, in priority order.
    served_by: Vec<String>,
    /// The candidate currently answering requests: the first of `served_by`
    /// that's reachable. `None` for a replicated drive, or when every
    /// candidate is unreachable.
    active_server: Option<String>,
    sync_interval: String,
    /// Nodes that implement this drive, with whether each one is currently
    /// reachable.
    nodes: Vec<DriveNodeStatus>,
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
    let cluster_state = state.cluster.state().await;
    Json(
        cluster_config
            .drives
            .iter()
            .map(|(name, drive)| DriveSummary {
                name: name.clone(),
                mode: drive.mode,
                served_by: drive.served_by.clone(),
                active_server: drive
                    .served_by
                    .iter()
                    .find(|n| !cluster_state.unreachable.contains(*n))
                    .cloned(),
                sync_interval: humantime::format_duration(drive.sync_interval).to_string(),
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
    /// The source found no files in it on two scans in a row.
    looks_empty: bool,
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
    let cluster_state = state.cluster.state().await;
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
            })
            .collect(),
    )
}

/// Triggering a sync outside the drive's own schedule is not a raft command:
/// `cl-drive` owns the actual sync loop and this crate has no channel to
/// nudge it directly. `TouchVolume` bumps the generation so the drive's next
/// pass (which watches for exactly that) picks the volume up right away.
async fn sync_volume(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path((drive, volume)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let id = VolumeId::new(drive, volume);
    let cluster_state = state.cluster.state().await;
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
    /// The node whose copy was looked at.
    node: String,
    files: usize,
    bytes: u64,
    /// Modification time of the newest file, unix seconds. `None` when there
    /// is no file at all.
    newest_file_at: Option<i64>,
}

/// What a volume holds right now, read from its source node on demand: the
/// cluster only records whether it looks empty, not how much is in it.
async fn inspect_volume(
    _identity: Identity,
    State(state): State<AppState>,
    Path((drive, volume)): Path<(String, String)>,
) -> Result<Json<VolumeInspection>, ApiError> {
    let id = VolumeId::new(drive, volume);
    let cluster_state = state.cluster.state().await;
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
    let source = match served_by {
        Some(candidates) => candidates
            .into_iter()
            .find(|n| !cluster_state.unreachable.contains(n))
            .ok_or_else(|| ApiError::Conflict(format!("no node is serving {}", id.drive)))?,
        None => record.primary.clone(),
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

/// Deletes a volume that holds no files, on every node. The cluster refuses
/// unless the volume is flagged empty, is not frozen and is not mounted.
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
