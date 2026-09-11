//! `/api/self-update/*`: release settings, run control, and the node-to-node
//! internal route that triggers a restart.
//!
//! The GitHub token is optional and write-only, like the Cloudflare token
//! (`routes::cloudflare::set_token` is the template): it's only needed for a
//! private repo or to avoid GitHub's low unauthenticated rate limit. The
//! release url is write-once like the Cloudflare token too.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use cl_config::SelfUpdateConfig;
use cl_raft::Request as ClusterRequest;
use cl_secret::Sealed;
use cl_update::orchestrator::load_run;
use cl_update::plan::{UpdateRunState, GITHUB_TOKEN_SECRET, RUN_CONFIG};
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
    name == GITHUB_TOKEN_SECRET
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/self-update/github-token",
            get(github_token_status).put(set_github_token),
        )
        .route("/api/self-update/repo", put(set_repo).get(get_repo))
        .route("/api/self-update/run", post(start_run).get(get_run))
        .route("/api/self-update/cancel", post(cancel_run))
        .route("/api/self-update/internal/restart", post(internal_restart))
}

#[derive(Serialize)]
struct GithubTokenStatus {
    /// True once a token is stored, which is what the ui keys the "you cannot
    /// see or change this here" state off.
    configured: bool,
}

async fn github_token_status(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<GithubTokenStatus> {
    let cluster_state = state.cluster.state().await;
    Json(GithubTokenStatus {
        configured: cluster_state.secrets.contains_key(GITHUB_TOKEN_SECRET),
    })
}

#[derive(Deserialize)]
struct SetGithubTokenRequest {
    token: String,
}

/// Seals and stores the token, replacing whatever was there before.
async fn set_github_token(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<SetGithubTokenRequest>,
) -> Result<StatusCode, ApiError> {
    let token = body.token.trim().to_string();
    if token.is_empty() {
        return Err(ApiError::BadRequest("the token is empty".into()));
    }

    let key_guard = state.key.read().await;
    let key = key_guard
        .as_ref()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;
    let sealed =
        Sealed::seal(key, GITHUB_TOKEN_SECRET, token.as_bytes()).map_err(ApiError::internal)?;
    let key_fingerprint = key.fingerprint();
    drop(key_guard);

    let response = state
        .cluster
        .submit(ClusterRequest::PutSecret {
            name: GITHUB_TOKEN_SECRET.to_string(),
            sealed: sealed.blob,
            key_fingerprint,
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SetRepoRequest {
    release_url: String,
}

/// Write-once, exactly like `cloudflare::set_token`: further changes go
/// through the config file by hand, not back through this route.
async fn set_repo(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<SetRepoRequest>,
) -> Result<StatusCode, ApiError> {
    if body.release_url.trim().is_empty() {
        return Err(ApiError::BadRequest("release_url is required".into()));
    }

    let mut config = state.cluster_config.read().await.clone();
    if config.self_update.is_some() {
        return Err(ApiError::Conflict(
            "a self-update release is already configured; edit the config file to change it".into(),
        ));
    }

    config.self_update = Some(SelfUpdateConfig {
        release_url: body.release_url.trim().to_string(),
    });
    super::config::apply_config(&state, &config).await?;
    Ok(StatusCode::OK)
}

async fn get_repo(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Option<SelfUpdateConfig>> {
    Json(state.cluster_config.read().await.self_update.clone())
}

async fn start_run(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> Result<Json<UpdateRunState>, ApiError> {
    let config = state.cluster_config.read().await;
    if config.self_update.is_none() {
        return Err(ApiError::Conflict(
            "no self-update release is configured".into(),
        ));
    }
    drop(config);

    let cluster_state = state.cluster.state().await;
    if let Some(existing) = load_run(&cluster_state) {
        if existing.in_progress() {
            return Err(ApiError::Conflict(
                "a self-update run is already in progress".into(),
            ));
        }
    }

    let id = format!("run-{}", now());
    let run = UpdateRunState::new(id, cluster_state.nodes.keys().cloned(), now());
    submit_run(&state, &run).await?;
    Ok(Json(run))
}

async fn get_run(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Option<UpdateRunState>> {
    let cluster_state = state.cluster.state().await;
    Json(load_run(&cluster_state))
}

async fn cancel_run(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
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

/// Swap in the artifact this node already downloaded and trigger the same
/// clean shutdown `SIGINT` drives. The unit's `Restart=always` brings the
/// process back up running the new binary.
async fn internal_restart(
    _auth: InternalAuth,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    let current_exe = std::env::current_exe().map_err(ApiError::internal)?;
    cl_update::install::install_binary(&state.self_update_artifact_path, &current_exe)
        .map_err(ApiError::internal)?;
    let _ = state.shutdown.send(());
    Ok(StatusCode::ACCEPTED)
}
