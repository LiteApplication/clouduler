//! `/api/restic/*`: restic-based drive backups - destinations, per-drive
//! schedules, run history, snapshots, and restore.
//!
//! Manual runs are a plain `RecordBackupRun` submit (`clouduler`'s
//! `backup_runner::start_runner` polls for `Running` runs on its own, so
//! there is no nudge endpoint the way `drives::sync_volume` needs one).
//! Restore is different: it needs a real local drive/docker to act on, which
//! this crate deliberately never has direct access to (see `drives.rs`'s doc
//! comment on the same constraint). So restore submits a `RestoreRequest`
//! through raft instead of doing any IO itself; `clouduler`'s
//! `backup_restore::start_restore_watcher` on the node that actually holds
//! the drive claims and executes it. `/api/events` already streams
//! `restore_requests`/`restic_runs` on every state version bump, so the ui
//! polls status from there rather than a bespoke SSE stream here.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_raft::ids::VolumeId;
use cl_raft::state::{
    BackupDestination, BackupDestinationKind, BackupRun, BackupRunOutcome, BackupTrigger,
    DriveBackupTarget, RestoreRequest, RestoreScope, RestoreStatus,
};
use cl_raft::Request as ClusterRequest;
use cl_restic::restic::SnapshotInfo;
use cl_secret::Sealed;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/restic/destinations",
            get(list_destinations).put(put_destination),
        )
        .route(
            "/api/restic/destinations/{id}",
            axum::routing::delete(remove_destination),
        )
        .route("/api/restic/targets", get(list_targets).put(put_target))
        .route(
            "/api/restic/targets/{id}",
            axum::routing::delete(remove_target),
        )
        .route("/api/restic/targets/{id}/run", post(run_target))
        .route("/api/restic/runs", get(list_runs))
        .route(
            "/api/restic/destinations/{id}/snapshots",
            get(list_snapshots),
        )
        .route("/api/restic/restore/drive", post(restore_drive))
        .route("/api/restic/restore/volume", post(restore_volume))
        .route("/api/restic/restore/service", post(restore_service))
        .route("/api/restic/restore-requests", get(list_restore_requests))
}

#[derive(Serialize)]
struct DestinationSummary {
    id: String,
    name: String,
    kind: BackupDestinationKind,
    has_credentials: bool,
}

async fn list_destinations(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Vec<DestinationSummary>> {
    let cluster_state = state.cluster.state().await;
    Json(
        cluster_state
            .restic_destinations
            .values()
            .map(|d| DestinationSummary {
                id: d.id.clone(),
                name: d.name.clone(),
                kind: d.kind.clone(),
                has_credentials: d.credentials_secret.is_some(),
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct PutDestinationRequest {
    id: String,
    name: String,
    kind: BackupDestinationKind,
    /// Repo password. Omitted (on an existing destination) keeps the current
    /// one; required the first time a destination is created.
    #[serde(default)]
    password: Option<String>,
    /// `Ssh`: PEM-encoded private key. `S3`: ignored.
    #[serde(default)]
    ssh_private_key: Option<String>,
    /// `S3`: access key id, paired with `s3_secret_access_key`.
    #[serde(default)]
    s3_access_key_id: Option<String>,
    #[serde(default)]
    s3_secret_access_key: Option<String>,
}

async fn put_destination(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<PutDestinationRequest>,
) -> Result<StatusCode, ApiError> {
    // Before anything is sealed: a rejected destination must not leave
    // orphaned secrets behind. The state machine checks the same rules.
    let probe = BackupDestination {
        id: body.id.clone(),
        name: body.name.clone(),
        kind: body.kind.clone(),
        password_secret: String::new(),
        credentials_secret: None,
    };
    cl_raft::backup_validate::validate_destination(&probe).map_err(ApiError::BadRequest)?;

    let cluster_state = state.cluster.state().await;
    let existing = cluster_state.restic_destinations.get(&body.id).cloned();

    let key_guard = state.key.read().await;
    let key = key_guard
        .as_ref()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;

    let password_secret = cl_restic::secret_names::password_secret_name(&body.id);
    match body.password {
        Some(password) => {
            seal_and_submit(&state, key, &password_secret, password.as_bytes()).await?
        }
        None if existing.is_some() => {}
        None => {
            return Err(ApiError::BadRequest(
                "password is required for a new destination".into(),
            ))
        }
    }

    let credentials_secret_name = cl_restic::secret_names::credentials_secret_name(&body.id);
    let credentials_secret = match &body.kind {
        BackupDestinationKind::Ssh { .. } => match body.ssh_private_key {
            Some(pem) => {
                seal_and_submit(&state, key, &credentials_secret_name, pem.as_bytes()).await?;
                Some(credentials_secret_name)
            }
            None => existing.as_ref().and_then(|d| d.credentials_secret.clone()),
        },
        BackupDestinationKind::S3 { .. } => {
            match (body.s3_access_key_id, body.s3_secret_access_key) {
                (Some(access_key), Some(secret_key)) => {
                    let plaintext = format!("{access_key}\n{secret_key}");
                    seal_and_submit(&state, key, &credentials_secret_name, plaintext.as_bytes())
                        .await?;
                    Some(credentials_secret_name)
                }
                _ => existing.as_ref().and_then(|d| d.credentials_secret.clone()),
            }
        }
        BackupDestinationKind::NodeLocal { .. } => None,
    };

    let response = state
        .cluster
        .submit(ClusterRequest::PutBackupDestination(BackupDestination {
            id: body.id,
            name: body.name,
            kind: body.kind,
            password_secret,
            credentials_secret,
        }))
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }
    Ok(StatusCode::OK)
}

async fn seal_and_submit(
    state: &AppState,
    key: &cl_secret::ClusterKey,
    name: &str,
    plaintext: &[u8],
) -> Result<(), ApiError> {
    let sealed = Sealed::seal(key, name, plaintext).map_err(ApiError::internal)?;
    let response = state
        .cluster
        .submit(ClusterRequest::PutSecret {
            name: name.to_string(),
            sealed: sealed.blob,
            key_fingerprint: key.fingerprint(),
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }
    Ok(())
}

async fn remove_destination(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let response = state
        .cluster
        .submit(ClusterRequest::RemoveBackupDestination { id })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn list_targets(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Vec<DriveBackupTarget>> {
    let cluster_state = state.cluster.state().await;
    Json(cluster_state.restic_targets.values().cloned().collect())
}

#[derive(Deserialize)]
struct PutTargetRequest {
    id: String,
    drive: String,
    destination_id: String,
    enabled: bool,
    frequency_secs: u64,
    #[serde(default)]
    time_of_day: Option<String>,
    #[serde(default)]
    keep_last: Option<u32>,
    #[serde(default)]
    keep_within_secs: Option<u64>,
    #[serde(default)]
    stop_containers: Option<bool>,
}

async fn put_target(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<PutTargetRequest>,
) -> Result<StatusCode, ApiError> {
    if body.frequency_secs == 0 {
        return Err(ApiError::BadRequest(
            "frequency_secs must be at least 1".into(),
        ));
    }
    let cluster_state = state.cluster.state().await;
    // Preserve last_run across an edit - it isn't part of the request body.
    let last_run = cluster_state
        .restic_targets
        .get(&body.id)
        .and_then(|t| t.last_run);

    let response = state
        .cluster
        .submit(ClusterRequest::PutBackupTarget(DriveBackupTarget {
            id: body.id,
            drive: body.drive,
            destination_id: body.destination_id,
            enabled: body.enabled,
            frequency_secs: body.frequency_secs,
            time_of_day: body.time_of_day,
            keep_last: body.keep_last,
            keep_within_secs: body.keep_within_secs,
            stop_containers: body.stop_containers,
            last_run,
        }))
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }
    Ok(StatusCode::OK)
}

async fn remove_target(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let response = state
        .cluster
        .submit(ClusterRequest::RemoveBackupTarget { id })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Trigger a run immediately, outside its schedule. `clouduler`'s
/// `backup_runner::start_runner` polls for `Running` runs on its own, so
/// this is a plain submit with no daemon-side nudge needed.
async fn run_target(
    admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<String>, ApiError> {
    let cluster_state = state.cluster.state().await;
    let target = cluster_state
        .restic_targets
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("no backup target {id}")))?;

    // The state machine rejects a second `Running` run for the same target
    // too (the authoritative check - this one's just a friendlier error
    // before paying for a raft round trip in the common case).
    if cluster_state
        .restic_runs
        .values()
        .any(|r| r.target_id == id && matches!(r.outcome, BackupRunOutcome::Running { .. }))
    {
        return Err(ApiError::Conflict(format!(
            "a backup is already running for target {id}"
        )));
    }

    let run_id = Uuid::new_v4().to_string();
    let response = state
        .cluster
        .submit(ClusterRequest::RecordBackupRun(BackupRun {
            id: run_id.clone(),
            target_id: id,
            drive: target.drive.clone(),
            destination_id: target.destination_id.clone(),
            started_at: unix_now(),
            finished_at: None,
            outcome: BackupRunOutcome::Running { percent: None },
            triggered_by: BackupTrigger::Manual {
                by: admin.0.username,
            },
        }))
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(Json(run_id))
}

#[derive(Deserialize)]
struct RunsQuery {
    target: Option<String>,
}

async fn list_runs(
    _identity: Identity,
    State(state): State<AppState>,
    Query(query): Query<RunsQuery>,
) -> Json<Vec<BackupRun>> {
    let cluster_state = state.cluster.state().await;
    let mut runs: Vec<BackupRun> = cluster_state
        .restic_runs
        .values()
        .filter(|r| query.target.as_deref().is_none_or(|t| r.target_id == t))
        .cloned()
        .collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    Json(runs)
}

async fn list_snapshots(
    _identity: Identity,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<SnapshotInfo>>, ApiError> {
    let cluster_state = state.cluster.state().await;
    let destination = cluster_state
        .restic_destinations
        .get(&id)
        .ok_or_else(|| ApiError::NotFound(format!("no backup destination {id}")))?
        .clone();

    let key = {
        let key_guard = state.key.read().await;
        key_guard
            .as_ref()
            .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?
            .clone()
    };

    // The same repository builder the runner and restore use, so S3 keys and
    // ssh identities are wired up too - and credentials are staged under the
    // data directory (0700, unique per call), never a predictable /tmp path.
    let cred_dir = state.data_dir.join("restic").join("creds").join(&id);
    let (repo, guards) =
        cl_restic::credentials::build_repo(&cluster_state, &key, &cred_dir, &destination)
            .map_err(|e| ApiError::Conflict(e.to_string()))?;
    let list = tokio::task::spawn_blocking(move || {
        let result = repo.snapshots();
        // Staged credentials go away as soon as restic is done with them.
        drop(guards);
        result
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(|e| ApiError::Internal(format!("listing snapshots: {e}")))?;

    Ok(Json(list))
}

#[derive(Deserialize)]
struct RestoreDriveRequest {
    drive: String,
    destination_id: String,
    #[serde(default)]
    snapshot: Option<String>,
}

async fn restore_drive(
    admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<RestoreDriveRequest>,
) -> Result<Json<String>, ApiError> {
    submit_restore(
        &state,
        RestoreScope::Drive { drive: body.drive },
        Some(body.destination_id),
        body.snapshot,
        admin.0.username,
    )
    .await
}

#[derive(Deserialize)]
struct RestoreVolumeRequest {
    drive: String,
    volume: String,
    destination_id: String,
    #[serde(default)]
    snapshot: Option<String>,
}

async fn restore_volume(
    admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<RestoreVolumeRequest>,
) -> Result<Json<String>, ApiError> {
    submit_restore(
        &state,
        RestoreScope::Volume {
            volume: VolumeId::new(body.drive, body.volume),
        },
        Some(body.destination_id),
        body.snapshot,
        admin.0.username,
    )
    .await
}

#[derive(Deserialize)]
struct RestoreServiceRequest {
    stack: String,
    service: String,
    #[serde(default)]
    destination_id: Option<String>,
    #[serde(default)]
    snapshot: Option<String>,
}

async fn restore_service(
    admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<RestoreServiceRequest>,
) -> Result<Json<String>, ApiError> {
    submit_restore(
        &state,
        RestoreScope::Service {
            stack: body.stack,
            service: body.service,
        },
        body.destination_id,
        body.snapshot,
        admin.0.username,
    )
    .await
}

async fn submit_restore(
    state: &AppState,
    scope: RestoreScope,
    destination_id: Option<String>,
    snapshot: Option<String>,
    requested_by: String,
) -> Result<Json<String>, ApiError> {
    if let Some(snapshot) = &snapshot {
        cl_restic::restore::validate_snapshot_id(snapshot).map_err(ApiError::BadRequest)?;
    }
    let id = Uuid::new_v4().to_string();
    let response = state
        .cluster
        .submit(ClusterRequest::PutRestoreRequest(RestoreRequest {
            id: id.clone(),
            scope,
            destination_id,
            snapshot,
            requested_at: unix_now(),
            finished_at: None,
            status: RestoreStatus::Pending,
            requested_by,
        }))
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }
    Ok(Json(id))
}

async fn list_restore_requests(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Vec<RestoreRequest>> {
    let cluster_state = state.cluster.state().await;
    let mut requests: Vec<RestoreRequest> =
        cluster_state.restore_requests.values().cloned().collect();
    requests.sort_by_key(|r| std::cmp::Reverse(r.requested_at));
    Json(requests)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
