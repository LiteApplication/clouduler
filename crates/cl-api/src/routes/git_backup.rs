//! `GET /api/git-backup/status`: read-only view of the git backup feature.
//!
//! Unlike `self_update`'s write-once `PUT /api/self-update/repo`, `backup:`'s
//! fields (repo url, age recipient, branch, interval) are meant to be freely
//! editable, so they go through the existing `PUT /api/config` draft-editing
//! flow the ui already uses for `FailoverConfig`/`SecretsConfig` - this route
//! exists only to report what the leader has actually done with them.
//!
//! Also `POST /api/git-backup/webhook`: the reverse direction. A git host
//! notifies this endpoint after a push; the leader fetches the tracked
//! branch, and if the new tip parses and validates cleanly, applies it to the
//! cluster (cluster config, stacks, and the plain configs they reference -
//! never secrets, see `validate_parsed` below). A commit that fails
//! validation is never applied: it is preserved on its own `rejected/...`
//! branch and the tracked branch is force-pushed back to its last known-good
//! state with an empty commit on top explaining why, so the remote is never
//! left pointing at something this cluster refuses to run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_backup::deploy_key::{public_from_state, DEPLOY_KEY_PRIVATE_SECRET, DEPLOY_KEY_PUBLIC_CONFIG};
use cl_backup::status::{load as load_last_export, load_pull, LastExport, LastPull, RejectedPull, LAST_EXPORT_CONFIG, LAST_PULL_CONFIG};
use cl_backup::tree::ParsedTree;
use cl_config::{ClusterConfig, StackFile};
use cl_raft::Request as ClusterRequest;
use cl_secret::Sealed;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::stacks::deploy_from_files;
use crate::auth::internal::InternalAuth;
use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::internal_peer;
use crate::server::AppState;

/// Reserved secret this feature's webhook is authenticated with - a shared
/// token the admin pastes into both this cluster and the git host's webhook
/// config. Sealed like any other secret; there is no getter for its value.
pub const WEBHOOK_SECRET: &str = "_clouduler_git_backup_webhook_secret";

/// Config/secret names this feature owns and never shows through the
/// generic config/secret listing routes, mirroring `self_update`'s.
pub fn is_reserved_config(name: &str) -> bool {
    name == DEPLOY_KEY_PUBLIC_CONFIG || name == LAST_EXPORT_CONFIG || name == LAST_PULL_CONFIG
}

pub fn is_reserved_secret(name: &str) -> bool {
    name == DEPLOY_KEY_PRIVATE_SECRET || name == WEBHOOK_SECRET
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/git-backup/status", get(status))
        .route("/api/git-backup/webhook-secret", get(webhook_secret_status).put(set_webhook_secret))
        .route("/api/git-backup/webhook", post(webhook))
        .route("/api/git-backup/webhook/internal-trigger", post(internal_trigger))
}

#[derive(Serialize)]
struct GitBackupStatus {
    /// True once `backup:` is set in the cluster config.
    configured: bool,
    /// The public half of the deploy key, once the leader has generated
    /// one - hand this to the git host as a write-capable deploy key.
    deploy_public_key: Option<String>,
    last_export: Option<LastExport>,
    last_pull: Option<LastPull>,
}

async fn status(_identity: Identity, State(state): State<AppState>) -> Json<GitBackupStatus> {
    let configured = state.cluster_config.read().await.backup.is_some();
    let cluster_state = state.cluster.state().await;
    Json(GitBackupStatus {
        configured,
        deploy_public_key: public_from_state(&cluster_state),
        last_export: load_last_export(&cluster_state),
        last_pull: load_pull(&cluster_state),
    })
}

#[derive(Serialize)]
struct WebhookSecretStatus {
    configured: bool,
}

async fn webhook_secret_status(_identity: Identity, State(state): State<AppState>) -> Json<WebhookSecretStatus> {
    let cluster_state = state.cluster.state().await;
    Json(WebhookSecretStatus { configured: cluster_state.secrets.contains_key(WEBHOOK_SECRET) })
}

#[derive(Deserialize)]
struct SetWebhookSecretRequest {
    token: String,
}

/// Seals and stores the token, replacing whatever was there before. There is
/// no getter for its value, same reasoning as `self_update`'s github token.
async fn set_webhook_secret(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<SetWebhookSecretRequest>,
) -> Result<StatusCode, ApiError> {
    let token = body.token.trim().to_string();
    if token.is_empty() {
        return Err(ApiError::BadRequest("the token is empty".into()));
    }

    let key_guard = state.key.read().await;
    let key = key_guard
        .as_ref()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;
    let sealed = Sealed::seal(key, WEBHOOK_SECRET, token.as_bytes()).map_err(ApiError::internal)?;
    let key_fingerprint = key.fingerprint();
    drop(key_guard);

    let response = state
        .cluster
        .submit(ClusterRequest::PutSecret {
            name: WEBHOOK_SECRET.to_string(),
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

/// `X-Gitlab-Token` is sent verbatim by GitLab's own webhook config, which is
/// exactly the "static shared secret in a header" shape this feature wants;
/// `Authorization: Bearer` is accepted too for hosts (or a proxy in front of
/// one) that only offer that.
fn presented_token(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-gitlab-token").and_then(|v| v.to_str().ok()) {
        return Some(v.to_string());
    }
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn verify_webhook_token(state: &AppState, presented: &str) -> bool {
    let cluster_state = state.cluster.state().await;
    let Some(entry) = cluster_state.secrets.get(WEBHOOK_SECRET) else {
        return false;
    };
    let key_guard = state.key.read().await;
    let Some(key) = key_guard.as_ref() else {
        return false;
    };
    let sealed = Sealed::from_blob(entry.sealed.clone());
    match sealed.open(key, WEBHOOK_SECRET) {
        Ok(expected) => constant_time_eq(&expected, presented.as_bytes()),
        Err(_) => false,
    }
}

/// The node currently holding raft leadership, by name - `internal_peer`
/// addresses peers by name, but raft only knows leadership by `NodeId`.
async fn leader_node_name(state: &AppState) -> Option<String> {
    let leader_id = state.cluster.quorum().leader()?;
    let cfg = state.cluster_config.read().await;
    cfg.nodes
        .iter()
        .find(|(_, node)| cl_raft::node_id(node.id) == leader_id)
        .map(|(name, _)| name.clone())
}

/// Public, token-authenticated endpoint a git host's webhook config points
/// at. Any node can receive it; a non-leader forwards the trigger to
/// whichever node currently leads, the same HMAC-authenticated channel
/// `internal_peer` uses for every other node-to-node call, since the actual
/// git working copy this needs only exists on the leader.
async fn webhook(State(state): State<AppState>, headers: HeaderMap) -> Result<StatusCode, ApiError> {
    let presented = presented_token(&headers).ok_or_else(|| ApiError::Unauthenticated("missing webhook token".into()))?;
    if !verify_webhook_token(&state, &presented).await {
        return Err(ApiError::Unauthenticated("bad webhook token".into()));
    }

    if !state.cluster.is_leader() {
        let leader = leader_node_name(&state)
            .await
            .ok_or_else(|| ApiError::Unavailable("no cluster leader known right now".into()))?;
        internal_peer::post_empty(&state, &leader, "/api/git-backup/webhook/internal-trigger").await?;
        return Ok(StatusCode::NO_CONTENT);
    }

    run_pull(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Node-to-node only: what `webhook` forwards to when this node is not the
/// leader.
async fn internal_trigger(_auth: InternalAuth, State(state): State<AppState>) -> Result<StatusCode, ApiError> {
    run_pull(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// What the blocking git2 half of a pull decided, handed back to async code
/// to either apply (raft submissions) or just report.
enum PullDecision {
    /// Fetched tip is the same as the tracked branch's tip - nothing to do.
    NoChange,
    Rejected {
        reason: String,
        preserved_branch: String,
    },
    Accepted {
        candidate: git2::Oid,
        cluster_config: Box<ClusterConfig>,
        parsed: Box<ParsedTree>,
    },
}

async fn run_pull(state: &AppState) -> Result<(), ApiError> {
    let backup_cfg = state
        .cluster_config
        .read()
        .await
        .backup
        .clone()
        .ok_or_else(|| ApiError::Conflict("git backup is not configured".into()))?;

    let cluster_state = state.cluster.state().await;
    let key = {
        let key_guard = state.key.read().await;
        key_guard
            .as_ref()
            .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?
            .clone()
    };
    let private_key_pem = cl_backup::deploy_key::load_private(&cluster_state, &key)
        .ok_or_else(|| ApiError::Conflict("no git deploy key yet - it is generated after the first export".into()))?;
    let existing_secrets: BTreeSet<String> = cluster_state.secrets.keys().cloned().collect();

    let dir = state.data_dir.join("git-backup-pull");
    let decision = tokio::task::spawn_blocking(move || pull_and_decide(&dir, &backup_cfg, &private_key_pem, &existing_secrets))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;

    match decision {
        PullDecision::NoChange => Ok(()),
        PullDecision::Rejected { reason, preserved_branch } => {
            warn!(
                %reason,
                %preserved_branch,
                "git backup: rejected a malformed pull, restored the tracked branch and preserved the bad commit separately"
            );
            publish_last_pull(
                state,
                LastPull {
                    at: unix_now(),
                    commit: None,
                    rejected: Some(RejectedPull { reason, preserved_branch }),
                },
            )
            .await
        }
        PullDecision::Accepted { candidate, cluster_config, parsed } => {
            apply_pulled_tree(state, &cluster_config, &parsed).await?;
            advance_local_ref(state, candidate).await?;

            publish_last_pull(
                state,
                LastPull {
                    at: unix_now(),
                    commit: Some(candidate.to_string()),
                    rejected: None,
                },
            )
            .await
        }
    }
}

/// Moves the pull working copy's local branch ref forward to `oid` now that
/// it has been applied, off the async executor for the same reason every
/// other git2 call here is.
async fn advance_local_ref(state: &AppState, oid: git2::Oid) -> Result<(), ApiError> {
    let backup_cfg = state.cluster_config.read().await.backup.clone();
    let Some(backup_cfg) = backup_cfg else {
        return Ok(());
    };
    let dir = state.data_dir.join("git-backup-pull");
    tokio::task::spawn_blocking(move || {
        let backup = cl_backup::git::GitBackup::open_or_init(&dir, &backup_cfg.repo_url, &backup_cfg.branch)?;
        backup.advance_local_ref(oid)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)
}

async fn publish_last_pull(state: &AppState, last_pull: LastPull) -> Result<(), ApiError> {
    let content = cl_raft::state::base64_encode(&serde_json::to_vec(&last_pull).map_err(ApiError::internal)?);
    let response = state
        .cluster
        .submit(ClusterRequest::PutConfig {
            name: LAST_PULL_CONFIG.to_string(),
            content,
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Internal(format!("publishing pull status: {msg}")));
    }
    Ok(())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// The blocking git2 half of a pull: fetch, decide whether the new tip is
/// usable, and if not, revert the remote right here (reverting is also git2
/// work, so it belongs in this same blocking closure rather than a second
/// round trip).
fn pull_and_decide(
    dir: &Path,
    cfg: &cl_config::GitBackupConfig,
    private_key_pem: &str,
    existing_secrets: &BTreeSet<String>,
) -> cl_backup::Result<PullDecision> {
    let backup = cl_backup::git::GitBackup::open_or_init(dir, &cfg.repo_url, &cfg.branch)?;
    let good = backup.local_branch_oid()?;
    let Some(candidate) = backup.fetch_remote_tip(private_key_pem)? else {
        return Ok(PullDecision::NoChange);
    };
    if Some(candidate) == good {
        return Ok(PullDecision::NoChange);
    }

    if backup.secrets_subtree_changed(good, candidate)? {
        let reason = "the pulled commit changes something under secrets/, which this cluster cannot decrypt back \
                       into a live secret - secret rotation must go through the api, not git"
            .to_string();
        let outcome = backup.reject_and_restore(candidate, good, &reason, private_key_pem)?;
        return Ok(PullDecision::Rejected { reason, preserved_branch: outcome.preserved_branch });
    }

    backup.checkout_tree_to_disk(candidate)?;
    let parsed = cl_backup::tree::parse_dir(dir)?;

    match validate_parsed(&parsed, existing_secrets) {
        Ok(cluster_config) => Ok(PullDecision::Accepted {
            candidate,
            cluster_config: Box::new(cluster_config),
            parsed: Box::new(parsed),
        }),
        Err(reason) => {
            let outcome = backup.reject_and_restore(candidate, good, &reason, private_key_pem)?;
            Ok(PullDecision::Rejected { reason, preserved_branch: outcome.preserved_branch })
        }
    }
}

/// Parse + validate only, no cluster state beyond `existing_secrets` (a
/// stack's `file_secrets` requirement can only ever be satisfied by a secret
/// this cluster already holds - git can never supply new secret content, see
/// the module docs). A raft-submission failure after this passes is not
/// treated as malformed; see the git-backup design notes.
fn validate_parsed(parsed: &ParsedTree, existing_secrets: &BTreeSet<String>) -> Result<ClusterConfig, String> {
    let Some(yaml) = &parsed.cluster_yaml else {
        return Err("no cluster.yaml in the pulled tree".to_string());
    };
    let cluster_config = ClusterConfig::parse(yaml).map_err(|e| format!("cluster.yaml: {e}"))?;

    for (name, spec) in &parsed.stacks {
        let stack = StackFile::parse(spec).map_err(|e| format!("stacks/{name}.yaml: {e}"))?;
        stack.validate(&cluster_config).map_err(|e| format!("stacks/{name}.yaml: {e}"))?;

        let requirements = stack.requirements();
        for config_name in &requirements.file_configs {
            if !parsed.configs.contains_key(config_name) {
                return Err(format!("stacks/{name}.yaml: config {config_name} has no content under configs/"));
            }
        }
        for secret_name in &requirements.file_secrets {
            if !existing_secrets.contains(secret_name) {
                return Err(format!(
                    "stacks/{name}.yaml: secret {secret_name} does not already exist in the cluster - git cannot \
                     supply new secret content, create it through the api first"
                ));
            }
        }
    }
    Ok(cluster_config)
}

/// Reconciles the cluster to match a validated pulled tree: the cluster
/// config, every stack (deployed or removed), and the plain configs they
/// reference (deployed, orphaned to no owner, or removed).
async fn apply_pulled_tree(state: &AppState, cluster_config: &ClusterConfig, parsed: &ParsedTree) -> Result<(), ApiError> {
    let current = state.cluster.state().await;
    let yaml_changed = current.cluster_config.as_ref().map(|c| c.yaml.as_str()) != parsed.cluster_yaml.as_deref();
    if yaml_changed {
        super::config::apply_config(state, cluster_config).await?;
    }

    let mut claimed_configs: BTreeSet<String> = BTreeSet::new();
    for (name, spec) in &parsed.stacks {
        let stack = StackFile::parse(spec).map_err(ApiError::internal)?;
        let requirements = stack.requirements();
        let mut configs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for config_name in &requirements.file_configs {
            claimed_configs.insert(config_name.clone());
            if let Some(bytes) = parsed.configs.get(config_name) {
                configs.insert(config_name.clone(), bytes.clone());
            }
        }
        deploy_from_files(state, name, spec.clone(), configs).await?;
    }

    let current = state.cluster.state().await;
    for name in current.stacks.keys() {
        if !parsed.stacks.contains_key(name) {
            let response = state
                .cluster
                .submit(ClusterRequest::RemoveStack { name: name.clone() })
                .await
                .map_err(ApiError::not_leader)?;
            if let Some(msg) = response.error {
                return Err(ApiError::Internal(format!("removing stack {name}: {msg}")));
            }
        }
    }

    for (name, bytes) in &parsed.configs {
        // Reserved configs (api keys, redirects, wireguard ingress, the
        // self-update run state, ...) get mirrored into the export for
        // backup purposes, but each has its own dedicated, validated write
        // path - a plain `PutConfig` of raw git content would bypass that
        // entirely and must never happen here.
        if claimed_configs.contains(name) || super::objects::is_reserved(name) {
            continue;
        }
        let response = state
            .cluster
            .submit(ClusterRequest::PutConfig {
                name: name.clone(),
                content: cl_backup::tree::encode_config_content(bytes),
                owner: None,
            })
            .await
            .map_err(ApiError::not_leader)?;
        if let Some(msg) = response.error {
            return Err(ApiError::Internal(format!("writing config {name}: {msg}")));
        }
    }

    let current = state.cluster.state().await;
    for name in current.configs.keys() {
        if cl_backup::snapshot::is_bookkeeping_config(name) || super::objects::is_reserved(name) {
            continue;
        }
        if !parsed.configs.contains_key(name) {
            let response = state
                .cluster
                .submit(ClusterRequest::RemoveConfig { name: name.clone() })
                .await
                .map_err(ApiError::not_leader)?;
            if let Some(msg) = response.error {
                return Err(ApiError::Internal(format!("removing config {name}: {msg}")));
            }
        }
    }

    Ok(())
}
