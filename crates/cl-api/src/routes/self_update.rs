//! `/api/self-update/*`: repo settings, run control, and the node-to-node
//! internal routes that move the built artifact and trigger a restart.
//!
//! Order of operations, enforced by the handlers below: generate the deploy
//! key first (`POST ssh-key`), add it as a deploy key on the git host by
//! hand, then set the repo (`PUT repo`), which is write-once like the
//! Cloudflare token (`routes::cloudflare::set_token` is the template).

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use cl_config::SelfUpdateConfig;
use cl_raft::Request as ClusterRequest;
use cl_secret::Sealed;
use cl_update::orchestrator::load_run;
use cl_update::plan::{UpdateRunState, DEPLOY_KEY_SECRET, RUN_CONFIG};
use serde::{Deserialize, Serialize};

use crate::auth::internal::InternalAuth;
use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::server::AppState;

/// Config names this feature owns and never shows through the generic
/// config/secret listing routes, mirroring `auth::keys::API_KEYS_CONFIG`.
pub fn is_reserved_config(name: &str) -> bool {
    name == RUN_CONFIG
}

pub fn is_reserved_secret(name: &str) -> bool {
    name == DEPLOY_KEY_SECRET
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/self-update/ssh-key", post(generate_ssh_key).get(get_ssh_key))
        .route("/api/self-update/repo", put(set_repo).get(get_repo))
        .route("/api/self-update/run", post(start_run).get(get_run))
        .route("/api/self-update/cancel", post(cancel_run))
        .route("/api/self-update/internal/artifact/{arch}", get(internal_artifact))
        .route("/api/self-update/internal/restart", post(internal_restart))
}

#[derive(Serialize)]
struct SshKeyResponse {
    public_key: String,
}

/// Idempotent: a second call recovers the same public key from the already
/// sealed private half rather than generating a new pair, so retrying a
/// timed out request never orphans the deploy key already added on the git
/// host.
async fn generate_ssh_key(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> Result<Json<SshKeyResponse>, ApiError> {
    let cluster_state = state.cluster.state().await;
    let key_guard = state.key.read().await;
    let key = key_guard
        .as_ref()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;

    if let Some(existing) = cluster_state.secrets.get(DEPLOY_KEY_SECRET) {
        let sealed = Sealed::from_blob(existing.sealed.clone());
        let plaintext = sealed.open(key, DEPLOY_KEY_SECRET).map_err(ApiError::internal)?;
        let private = String::from_utf8(plaintext).map_err(ApiError::internal)?;
        let public_key = cl_update::keypair::derive_public(&private).map_err(ApiError::internal)?;
        return Ok(Json(SshKeyResponse { public_key }));
    }

    let pair = cl_update::keypair::generate().map_err(ApiError::internal)?;
    let sealed = Sealed::seal(key, DEPLOY_KEY_SECRET, pair.private_openssh.as_bytes())
        .map_err(ApiError::internal)?;
    let key_fingerprint = key.fingerprint();
    drop(key_guard);

    let response = state
        .cluster
        .submit(ClusterRequest::PutSecret {
            name: DEPLOY_KEY_SECRET.to_string(),
            sealed: sealed.blob,
            key_fingerprint,
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }

    Ok(Json(SshKeyResponse { public_key: pair.public_openssh }))
}

#[derive(Serialize)]
struct SshKeyStatus {
    public_key: Option<String>,
}

/// The public key is only ever surfaced by `POST` before the repo is set:
/// once it is, the value copied into `ClusterConfig.self_update` is the one
/// place it can be read back from without holding the cluster key.
async fn get_ssh_key(_identity: Identity, State(state): State<AppState>) -> Json<SshKeyStatus> {
    let config = state.cluster_config.read().await;
    Json(SshKeyStatus {
        public_key: config.self_update.as_ref().map(|c| c.ssh_public_key.clone()),
    })
}

#[derive(Deserialize)]
struct SetRepoRequest {
    git_url: String,
    git_branch: String,
    ssh_public_key: String,
}

/// Write-once, exactly like `cloudflare::set_token`: further changes go
/// through the config file by hand, not back through this route.
async fn set_repo(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<SetRepoRequest>,
) -> Result<StatusCode, ApiError> {
    if body.git_url.trim().is_empty() || body.git_branch.trim().is_empty() {
        return Err(ApiError::BadRequest("git_url and git_branch are required".into()));
    }

    let mut config = state.cluster_config.read().await.clone();
    if config.self_update.is_some() {
        return Err(ApiError::Conflict(
            "a self-update repo is already configured; edit the config file to change it".into(),
        ));
    }

    config.self_update = Some(SelfUpdateConfig {
        git_url: body.git_url,
        git_branch: body.git_branch,
        ssh_public_key: body.ssh_public_key,
    });
    super::config::apply_config(&state, &config).await?;
    Ok(StatusCode::OK)
}

async fn get_repo(_identity: Identity, State(state): State<AppState>) -> Json<Option<SelfUpdateConfig>> {
    Json(state.cluster_config.read().await.self_update.clone())
}

async fn start_run(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> Result<Json<UpdateRunState>, ApiError> {
    let config = state.cluster_config.read().await;
    if config.self_update.is_none() {
        return Err(ApiError::Conflict("no self-update repo is configured".into()));
    }
    drop(config);

    let cluster_state = state.cluster.state().await;
    if let Some(existing) = load_run(&cluster_state) {
        if existing.in_progress() {
            return Err(ApiError::Conflict("a self-update run is already in progress".into()));
        }
    }

    let id = format!("run-{}", now());
    let run = UpdateRunState::new(id, cluster_state.nodes.keys().cloned(), now());
    submit_run(&state, &run).await?;
    Ok(Json(run))
}

async fn get_run(_identity: Identity, State(state): State<AppState>) -> Json<Option<UpdateRunState>> {
    let cluster_state = state.cluster.state().await;
    Json(load_run(&cluster_state))
}

async fn cancel_run(_admin: RequireAdmin, State(state): State<AppState>) -> Result<StatusCode, ApiError> {
    let cluster_state = state.cluster.state().await;
    let mut run = load_run(&cluster_state)
        .ok_or_else(|| ApiError::Conflict("no self-update run is on record".into()))?;
    if !run.in_progress() {
        return Err(ApiError::Conflict("this run has already finished".into()));
    }
    run.fail("cancelled by operator", now());
    submit_run(&state, &run).await?;
    Ok(StatusCode::OK)
}

async fn submit_run(state: &AppState, run: &UpdateRunState) -> Result<(), ApiError> {
    let content = cl_raft::base64_encode(&serde_json::to_vec(run).map_err(ApiError::internal)?);
    let response = state
        .cluster
        .submit(ClusterRequest::PutConfig {
            name: RUN_CONFIG.to_string(),
            content,
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }
    Ok(())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Raw bytes of the artifact this node currently holds staged for `arch`.
///
/// Serves whatever is on disk without checking it actually matches this
/// node's own reported arch: the peer asking already knows which node builds
/// its architecture, so there is nothing this node could sanity check that
/// the caller has not already decided.
async fn internal_artifact(
    _auth: InternalAuth,
    State(state): State<AppState>,
    Path(_arch): Path<String>,
) -> Result<Bytes, ApiError> {
    let bytes = tokio::fs::read(&state.self_update_artifact_path)
        .await
        .map_err(|_| ApiError::NotFound("no artifact staged on this node".into()))?;
    Ok(Bytes::from(bytes))
}

/// Swap in the artifact already fetched via `internal_artifact` (or built
/// locally) and trigger the same clean shutdown `SIGINT` drives. The unit's
/// `Restart=always` brings the process back up running the new binary.
async fn internal_restart(_auth: InternalAuth, State(state): State<AppState>) -> Result<StatusCode, ApiError> {
    let current_exe = std::env::current_exe().map_err(ApiError::internal)?;
    cl_update::install::install_binary(&state.self_update_artifact_path, &current_exe)
        .map_err(ApiError::internal)?;
    let _ = state.shutdown.send(());
    Ok(StatusCode::ACCEPTED)
}
