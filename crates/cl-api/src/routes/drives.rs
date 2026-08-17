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
}

#[derive(Serialize)]
struct DriveSummary {
    name: String,
    mode: String,
    served_by: Option<String>,
    /// Nodes that implement this drive, with whether each one is currently
    /// reachable.
    nodes: Vec<DriveNodeStatus>,
}

#[derive(Serialize)]
struct DriveNodeStatus {
    node: String,
    reachable: bool,
}

async fn list_drives(_identity: Identity, State(state): State<AppState>) -> Json<Vec<DriveSummary>> {
    let cluster_config = state.cluster_config.read().await;
    let cluster_state = state.cluster.state().await;
    Json(
        cluster_config
            .drives
            .iter()
            .map(|(name, drive)| DriveSummary {
                name: name.clone(),
                mode: format!("{:?}", drive.mode).to_lowercase(),
                served_by: drive.served_by.clone(),
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
}

#[derive(Serialize)]
struct ReplicaStatus {
    node: String,
    generation: u64,
    in_sync: bool,
}

async fn list_volumes(_identity: Identity, State(state): State<AppState>) -> Json<Vec<VolumeSummary>> {
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
