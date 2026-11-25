//! `/api/self-update/*`: release settings, run control, and the node-to-node route that triggers a restart.
//!
//! The GitHub token is optional and write-only, and the release url write-once, like the Cloudflare
//! token (`routes::cloudflare::set_token`). The token is only needed for a private repo or to avoid
//! GitHub's low unauthenticated rate limit.

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

pub fn is_reserved_config(name: &str) -> bool {
    name == RUN_CONFIG
        || name == cl_update::plan::CHECK_CONFIG
        || name == cl_update::reset::RESET_CONFIG
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
        .route("/api/self-update/check", put(set_check))
        .route("/api/self-update/latest", get(latest))
        .route("/api/self-update/releases", get(releases))
        .route("/api/self-update/run", post(start_run).get(get_run))
        .route("/api/self-update/cancel", post(cancel_run))
        .route("/api/self-update/internal/restart", post(internal_restart))
}

#[derive(Serialize)]
struct GithubTokenStatus {
    /// True once a token is stored; the ui keys its "cannot see or change this here" state off it.
    configured: bool,
}

async fn github_token_status(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<GithubTokenStatus> {
    let cluster_state = state.cluster.snapshot().await;
    Json(GithubTokenStatus {
        configured: cluster_state.secrets.contains_key(GITHUB_TOKEN_SECRET),
    })
}

#[derive(Deserialize)]
struct SetGithubTokenRequest {
    token: String,
}

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

/// Write-once, like `cloudflare::set_token`: later changes go through the config file.
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
        check_updates: true,
        check_interval: std::time::Duration::from_secs(24 * 60 * 60),
    });
    super::config::apply_config(&state, &config).await?;
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
struct SetCheckRequest {
    enabled: bool,
    #[serde(default)]
    interval: Option<String>,
}

async fn set_check(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<SetCheckRequest>,
) -> Result<StatusCode, ApiError> {
    let mut config = state.cluster_config.read().await.clone();
    let Some(self_update) = config.self_update.as_mut() else {
        return Err(ApiError::Conflict(
            "no self-update release is configured".into(),
        ));
    };
    if let Some(interval) = body.interval.as_deref() {
        let parsed = humantime::parse_duration(interval.trim())
            .map_err(|e| ApiError::BadRequest(format!("interval: {e}")))?;
        if parsed < std::time::Duration::from_secs(60 * 60) {
            return Err(ApiError::BadRequest(
                "interval must be at least 1h; GitHub rate-limits unauthenticated requests".into(),
            ));
        }
        self_update.check_interval = parsed;
    }
    self_update.check_updates = body.enabled;
    super::config::apply_config(&state, &config).await?;
    Ok(StatusCode::OK)
}

async fn get_repo(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Option<SelfUpdateConfig>> {
    Json(state.cluster_config.read().await.self_update.clone())
}

#[derive(Deserialize)]
struct StartRunRequest {
    #[serde(default)]
    tag: Option<String>,
    /// Explicit opt-in to install a tag older than the running version (unsupported).
    #[serde(default)]
    downgrade: bool,
}

/// Dotted numeric parts of a tag or version; a leading `v` is dropped and a non-numeric part counts as 0.
fn version_parts(v: &str) -> Vec<u64> {
    v.trim_start_matches('v')
        .split(['.', '-'])
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

/// True when `tag` names a strictly older release than `running`.
fn is_older(tag: &str, running: &str) -> bool {
    let (a, b) = (version_parts(tag), version_parts(running));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x < y;
        }
    }
    false
}

fn check_downgrade(tag: &str, running: &str, downgrade: bool) -> Result<(), ApiError> {
    if !downgrade && is_older(tag, running) {
        return Err(ApiError::BadRequest(format!(
            "tag {tag} is older than the running version {running}; downgrading is unsupported and must be requested explicitly"
        )));
    }
    Ok(())
}

async fn start_run(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Json<UpdateRunState>, ApiError> {
    let target_tag = if body.iter().all(u8::is_ascii_whitespace) {
        None
    } else {
        let req: StartRunRequest = serde_json::from_slice(&body)
            .map_err(|e| ApiError::BadRequest(format!("invalid body: {e}")))?;
        let downgrade = req.downgrade;
        match req.tag {
            None => None,
            Some(tag) => {
                let tag = tag.trim().to_string();
                if tag.is_empty() || tag.contains('/') || tag.contains(char::is_whitespace) {
                    return Err(ApiError::BadRequest(
                        "tag must be non-empty with no '/' or whitespace".into(),
                    ));
                }
                check_downgrade(&tag, cl_config::version(), downgrade)?;
                Some(tag)
            }
        }
    };
    let config = state.cluster_config.read().await;
    if config.self_update.is_none() {
        return Err(ApiError::Conflict(
            "no self-update release is configured".into(),
        ));
    }
    drop(config);

    let cluster_state = state.cluster.snapshot().await;
    if cl_update::reset::in_flight(&cluster_state) {
        return Err(ApiError::Conflict(
            "a configuration reset to a git commit is in progress".into(),
        ));
    }
    let existing = cl_update::orchestrator::load_run_revision(&cluster_state);
    let revision = existing.as_ref().map(|(_, r)| *r).unwrap_or(0);
    if let Some((existing, _)) = existing {
        if existing.in_progress() {
            return Err(ApiError::Conflict(
                "a self-update run is already in progress".into(),
            ));
        }
    }

    let id = format!("run-{}", now());
    let nodes_to_update = cluster_state
        .nodes
        .iter()
        .filter(|(_, n)| n.availability == cl_raft::state::Availability::Active)
        .map(|(name, _)| name.clone());
    let mut run = UpdateRunState::new(id, nodes_to_update, now());
    run.target_tag = target_tag;
    submit_run(&state, &run, revision).await?;
    Ok(Json(run))
}

async fn get_run(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Option<UpdateRunState>> {
    let cluster_state = state.cluster.snapshot().await;
    Json(load_run(&cluster_state))
}

async fn cancel_run(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    let cluster_state = state.cluster.snapshot().await;
    let (mut run, revision) = cl_update::orchestrator::load_run_revision(&cluster_state)
        .ok_or_else(|| ApiError::Conflict("no self-update run is on record".into()))?;
    if !run.in_progress() {
        return Err(ApiError::Conflict("this run has already finished".into()));
    }
    run.fail("cancelled by operator", now());
    submit_run(&state, &run, revision).await?;
    Ok(StatusCode::OK)
}

/// Stores `run` if the object is still at `revision`: the orchestrator and workers write it too. A lost race is a retryable 409.
pub(crate) async fn submit_run(
    state: &AppState,
    run: &UpdateRunState,
    revision: u64,
) -> Result<(), ApiError> {
    let request = cl_update::orchestrator::put_run_request(run, revision)
        .map_err(|e| ApiError::internal(std::io::Error::other(e)))?;
    let response = state
        .cluster
        .submit(request)
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(())
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Swaps in the downloaded artifact and triggers the clean shutdown `SIGINT` drives; the unit's `Restart=always` relaunches the new binary.
async fn internal_restart(
    _auth: InternalAuth,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    let current_exe = std::env::current_exe().map_err(ApiError::internal)?;
    let artifact = state.self_update_artifact_path.clone();
    let staged = artifact.clone();
    tokio::task::spawn_blocking(move || {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("preparing the downloaded binary: {e}"))?;
        cl_update::rollback::verify_runnable(&staged)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::Conflict)?;
    cl_update::install::install_binary(&artifact, &current_exe).map_err(ApiError::internal)?;
    if let Some(dir) = artifact.parent() {
        cl_update::rollback::arm(dir, &current_exe, now()).map_err(ApiError::internal)?;
    }
    // Shut down only after the response has left: when the leader restarts itself the caller is this process.
    let shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let _ = shutdown.send(());
    });
    Ok(StatusCode::ACCEPTED)
}

/// GitHub allows 60 unauthenticated requests an hour and the settings page polls.
const LATEST_TTL: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(Clone, Serialize)]
struct SelfUpdateLatest {
    tag: String,
    architectures: Vec<String>,
    source: String,
    running_version: String,
    fetched_at: i64,
}

type LatestCache = parking_lot::Mutex<Option<(std::time::Instant, SelfUpdateLatest)>>;

fn latest_cache() -> &'static LatestCache {
    static CACHE: std::sync::OnceLock<LatestCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

async fn latest(
    _identity: Identity,
    State(state): State<AppState>,
) -> Result<Json<SelfUpdateLatest>, ApiError> {
    let source = state
        .cluster_config
        .read()
        .await
        .self_update
        .as_ref()
        .map(|c| c.release_url.clone())
        .ok_or_else(|| ApiError::Conflict("no self-update release is configured".into()))?;
    if let Some((at, cached)) = latest_cache().lock().clone() {
        if at.elapsed() < LATEST_TTL && cached.source == source {
            return Ok(Json(cached));
        }
    }
    let token = unseal_github_token(&state).await;
    let resolved = cl_update::release::resolve(&source, token.as_deref())
        .await
        .map_err(ApiError::Unavailable)?;
    let answer = SelfUpdateLatest {
        tag: resolved.tag,
        architectures: resolved.assets.into_keys().collect(),
        source,
        running_version: cl_config::version().to_string(),
        fetched_at: now(),
    };
    *latest_cache().lock() = Some((std::time::Instant::now(), answer.clone()));
    Ok(Json(answer))
}

#[derive(Clone, Serialize)]
struct SelfUpdateReleases {
    tags: Vec<String>,
}

type ReleasesCache = parking_lot::Mutex<Option<(std::time::Instant, String, SelfUpdateReleases)>>;

fn releases_cache() -> &'static ReleasesCache {
    static CACHE: std::sync::OnceLock<ReleasesCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

async fn releases(
    _identity: Identity,
    State(state): State<AppState>,
) -> Result<Json<SelfUpdateReleases>, ApiError> {
    let source = state
        .cluster_config
        .read()
        .await
        .self_update
        .as_ref()
        .map(|c| c.release_url.clone())
        .ok_or_else(|| ApiError::Conflict("no self-update release is configured".into()))?;
    if let Some((at, cached_source, cached)) = releases_cache().lock().clone() {
        if at.elapsed() < LATEST_TTL && cached_source == source {
            return Ok(Json(cached));
        }
    }
    let token = unseal_github_token(&state).await;
    let tags = cl_update::release::list_tags(&source, token.as_deref())
        .await
        .map_err(ApiError::Unavailable)?;
    let answer = SelfUpdateReleases { tags };
    *releases_cache().lock() = Some((std::time::Instant::now(), source, answer.clone()));
    Ok(Json(answer))
}

pub(crate) async fn unseal_github_token(state: &AppState) -> Option<String> {
    let cluster_state = state.cluster.snapshot().await;
    let entry = cluster_state.secrets.get(GITHUB_TOKEN_SECRET)?;
    let sealed = Sealed::from_blob(entry.sealed.clone());
    let key = state.key.read().await.clone()?;
    let plaintext = sealed.open(&key, GITHUB_TOKEN_SECRET).ok()?;
    String::from_utf8(plaintext).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_tags_need_the_downgrade_flag() {
        assert!(check_downgrade("v0.18.2", "0.19.0", false).is_err());
        assert!(check_downgrade("v0.9.9", "0.10.0", false).is_err());
        assert!(check_downgrade("v0.18.2", "0.19.0", true).is_ok());
        assert!(check_downgrade("v0.19.0", "0.19.0", false).is_ok());
        assert!(check_downgrade("v0.19.1", "0.19.0", false).is_ok());
        assert!(check_downgrade("v0.19", "0.19.0", false).is_ok());
    }

    #[test]
    fn the_error_names_both_versions() {
        let ApiError::BadRequest(msg) = check_downgrade("v0.1.0", "0.2.0", false).unwrap_err()
        else {
            panic!("expected a 400");
        };
        assert_eq!(
            msg,
            "tag v0.1.0 is older than the running version 0.2.0; downgrading is unsupported and must be requested explicitly"
        );
    }
}
