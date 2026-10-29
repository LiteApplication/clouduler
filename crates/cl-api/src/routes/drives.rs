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
    /// Replication is paused so one node's copy can be edited; `None` when
    /// the volume replicates normally.
    frozen: Option<FrozenSummary>,
    /// The latest push a node refused because it would have overwritten
    /// newer data. `None` when nothing is refused.
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
                frozen: v.frozen.as_ref().map(|f| FrozenSummary {
                    node: f.node.clone(),
                    stopped: f.stopped.iter().map(|i| i.to_string()).collect(),
                }),
                sync_refused: v.sync_refused.clone(),
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

#[derive(Deserialize)]
struct InspectQuery {
    /// Look at this node's copy instead of the source node's. It must hold
    /// a replica of the volume.
    node: Option<String>,
}

/// What a volume holds right now, read from its source node on demand: the
/// cluster only records whether it looks empty, not how much is in it.
/// `?node=` asks a specific replica instead, to compare copies.
async fn inspect_volume(
    _identity: Identity,
    State(state): State<AppState>,
    Path((drive, volume)): Path<(String, String)>,
    Query(query): Query<InspectQuery>,
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

#[derive(Deserialize, Default)]
struct FreezeRequest {
    /// Node whose copy will be edited. Defaults to the volume's primary.
    node: Option<String>,
    /// Stop containers using the volume on other nodes.
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize, Default)]
struct UnfreezeRequest {
    /// Start the containers the freeze stopped. Default true.
    start_containers: Option<bool>,
}

/// Pauses replication so one node's copy can be edited. Stopping containers
/// on other nodes is requested but not waited for: the volume reads as frozen
/// straight away and the caller watches the instances stop.
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

/// Lifts the freeze and returns at once; waiting for every node to receive
/// the edited copy, then starting the stopped containers, carries on in the
/// background. The volume's replica generations show the progress.
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
