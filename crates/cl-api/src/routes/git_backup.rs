//! `GET /api/git-backup/status`: read-only view of the git backup feature.
//!
//! Also `POST /api/git-backup/webhook/{token}`: a git host notifies it after a push; the leader
//! fetches the tracked branch and, if the tip parses and validates, applies it (cluster config,
//! stacks, plain configs, never secrets; see `validate_parsed`). A failing commit is preserved on
//! its own `rejected/...` branch and the tracked branch is force-pushed back to the cluster state.
//!
//! The token is in the URL path because some git hosts cannot attach a custom header.
//!
//! `clouduler git-backup pull` uses `POST /api/git-backup/pull`, internal-auth (HMAC over the
//! cluster key) since the cli holds the key on disk.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_backup::deploy_key::{
    public_from_state, DEPLOY_KEY_PRIVATE_SECRET, DEPLOY_KEY_PUBLIC_CONFIG,
};
use cl_backup::git::{parse_tree, CommitInfo};
use cl_backup::status::{
    last_known_good_commit, load as load_last_export, load_pull, LastExport, LastPull,
    RejectedPull, LAST_EXPORT_CONFIG, LAST_PULL_CONFIG,
};
use cl_backup::tree::ParsedTree;
use cl_config::{validate_stack_name, ClusterConfig, StackFile};
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

/// Shared token pasted into both this cluster and the git host. Sealed; no getter.
pub const WEBHOOK_SECRET: &str = "_clouduler_git_backup_webhook_secret";

/// Names this feature owns and hides from the generic config/secret listings.
pub fn is_reserved_config(name: &str) -> bool {
    name == DEPLOY_KEY_PUBLIC_CONFIG || name == LAST_EXPORT_CONFIG || name == LAST_PULL_CONFIG
}

pub fn is_reserved_secret(name: &str) -> bool {
    name == DEPLOY_KEY_PRIVATE_SECRET || name == WEBHOOK_SECRET
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/git-backup/status", get(status))
        .route(
            "/api/git-backup/webhook-secret",
            get(webhook_secret_status).put(set_webhook_secret),
        )
        .route("/api/git-backup/webhook/{token}", post(webhook))
        .route(
            "/api/git-backup/webhook/internal-trigger",
            post(internal_trigger),
        )
        .route(
            "/api/git-backup/webhook/internal-reconcile",
            post(internal_reconcile),
        )
        .route("/api/git-backup/commits", get(list_commits))
        .route("/api/git-backup/rollback", post(rollback_admin))
        .route("/api/git-backup/internal-rollback", post(internal_rollback))
        .route("/api/git-backup/reset", post(reset_admin))
        .route("/api/git-backup/internal-reset", post(internal_reset))
        .route("/api/git-backup/internal-commits", get(internal_commits))
        .route("/api/git-backup/pull", post(pull_now))
        .route("/api/git-backup/reconcile", post(reconcile_now))
        // Admin-authenticated counterparts for the web ui (the three above are cli-only).
        .route("/api/git-backup/export-now", post(export_now_admin))
        .route("/api/git-backup/pull-now", post(pull_now_admin))
        .route("/api/git-backup/reconcile-now", post(reconcile_now_admin))
        .route(
            "/api/git-backup/internal-export-now",
            post(internal_export_now),
        )
}

/// Runs the export loop's next pass now; it only exports when something tracked changed. Leader-only, so forwarded.
async fn export_now_admin(
    admin: RequireAdmin,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    let email = format!("{}@localhost", admin.0.username);
    *state.git_export_author.lock() = Some((admin.0.username.clone(), email));
    if state.cluster.is_leader() {
        state.nudges.git_export.notify_one();
        return Ok(StatusCode::ACCEPTED);
    }
    let leader = leader_node_name(&state)
        .await
        .ok_or_else(|| ApiError::Unavailable("no cluster leader known right now".into()))?;
    let query = serde_urlencoded::to_string([("author", &admin.0.username)]).unwrap_or_default();
    let path = format!("/api/git-backup/internal-export-now?{query}");
    internal_peer::post_empty(&state, &leader, &path).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn internal_export_now(
    _auth: InternalAuth,
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> StatusCode {
    if let Some(username) = params.get("author") {
        let email = format!("{}@localhost", username);
        *state.git_export_author.lock() = Some((username.clone(), email));
    }
    state.nudges.git_export.notify_one();
    StatusCode::ACCEPTED
}

async fn pull_now_admin(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    trigger_pull(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn reconcile_now_admin(
    admin: RequireAdmin,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    let email = format!("{}@localhost", admin.0.username);
    trigger_reconcile(&state, Some((admin.0.username.clone(), email))).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct GitBackupStatus {
    configured: bool,
    /// Hand to the git host as a write-capable deploy key.
    deploy_public_key: Option<String>,
    last_export: Option<LastExport>,
    last_pull: Option<LastPull>,
}

async fn status(_identity: Identity, State(state): State<AppState>) -> Json<GitBackupStatus> {
    let configured = state.cluster_config.read().await.backup.is_some();
    let cluster_state = state.cluster.snapshot().await;
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

async fn webhook_secret_status(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<WebhookSecretStatus> {
    let cluster_state = state.cluster.snapshot().await;
    Json(WebhookSecretStatus {
        configured: cluster_state.secrets.contains_key(WEBHOOK_SECRET),
    })
}

#[derive(Deserialize)]
struct SetWebhookSecretRequest {
    token: String,
}

/// Replaces the stored token. No getter, as for `self_update`'s github token.
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

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn verify_webhook_token(state: &AppState, presented: &str) -> bool {
    let cluster_state = state.cluster.snapshot().await;
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

/// By name: `internal_peer` addresses peers by name, raft knows leadership by `NodeId`.
pub(crate) async fn leader_node_name(state: &AppState) -> Option<String> {
    let leader_id = state.cluster.quorum().leader()?;
    let cfg = state.cluster_config.read().await;
    cfg.nodes
        .iter()
        .find(|(_, node)| cl_raft::node_id(node.id) == leader_id)
        .map(|(name, _)| name.clone())
}

/// Public, token-authenticated. Any node can receive it; a non-leader forwards to the leader.
async fn webhook(
    State(state): State<AppState>,
    UrlPath(token): UrlPath<String>,
) -> Result<StatusCode, ApiError> {
    if !verify_webhook_token(&state, &token).await {
        return Err(ApiError::Unauthenticated("bad webhook token".into()));
    }
    trigger_pull(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The git work happens on the leader, so a non-leader forwards the trigger.
async fn trigger_pull(state: &AppState) -> Result<(), ApiError> {
    if !state.cluster.is_leader() {
        let leader = leader_node_name(state)
            .await
            .ok_or_else(|| ApiError::Unavailable("no cluster leader known right now".into()))?;
        return internal_peer::post_empty(
            state,
            &leader,
            "/api/git-backup/webhook/internal-trigger",
        )
        .await;
    }
    run_pull(state).await
}

async fn internal_trigger(
    _auth: InternalAuth,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    run_pull(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `clouduler git-backup pull`: the pull a webhook would trigger, without one configured. Internal auth.
async fn pull_now(
    _auth: InternalAuth,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    trigger_pull(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn trigger_reconcile(
    state: &AppState,
    author: Option<(String, String)>,
) -> Result<(), ApiError> {
    if !state.cluster.is_leader() {
        let leader = leader_node_name(state)
            .await
            .ok_or_else(|| ApiError::Unavailable("no cluster leader known right now".into()))?;
        let path = match &author {
            Some((name, _)) => {
                let query = serde_urlencoded::to_string([("author", name)]).unwrap_or_default();
                format!("/api/git-backup/webhook/internal-reconcile?{query}")
            }
            None => "/api/git-backup/webhook/internal-reconcile".to_string(),
        };
        return internal_peer::post_empty(state, &leader, &path).await;
    }
    run_reconcile(state, author).await
}

async fn internal_reconcile(
    _auth: InternalAuth,
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<StatusCode, ApiError> {
    let author = params
        .get("author")
        .map(|u| (u.clone(), format!("{}@localhost", u)));
    run_reconcile(&state, author).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `clouduler git-backup reconcile`: rebuilds cluster.yaml, stacks, configs and secrets from
/// raft, commits and pushes, and updates the last known good commit.
async fn reconcile_now(
    _auth: InternalAuth,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    trigger_reconcile(&state, None).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn run_reconcile(state: &AppState, author: Option<(String, String)>) -> Result<(), ApiError> {
    let backup_cfg = state
        .cluster_config
        .read()
        .await
        .backup
        .clone()
        .ok_or_else(|| ApiError::Conflict("git backup is not configured".into()))?;

    let cluster_state = state.cluster.snapshot().await;
    let key = {
        let key_guard = state.key.read().await;
        key_guard
            .as_ref()
            .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?
            .clone()
    };
    let private_key_pem =
        cl_backup::deploy_key::load_private(&cluster_state, &key).ok_or_else(|| {
            ApiError::Conflict(
                "no git deploy key yet - it is generated after the first export".into(),
            )
        })?;
    let recipient = cl_backup::age::parse_recipient(&backup_cfg.age_recipient)
        .map_err(|e| ApiError::Conflict(format!("invalid age recipient: {e}")))?;

    let cache_path = state.data_dir.join("git_backup_cache.json");
    let mut cache_state = cl_backup::cache_state::CacheState::load(&cache_path);
    let dir = state.data_dir.join("git-backup");
    let cluster_state_for_build = cluster_state.clone();
    let mut secret_cache = std::mem::take(&mut cache_state.secret_cache);
    let author_owned = author;
    let (commit_oid, secret_cache) = tokio::task::spawn_blocking(move || {
        let _repo_guard = cl_backup::git::lock_repo();
        let backup = cl_backup::git::GitBackup::open_or_init(
            &dir,
            &backup_cfg.repo_url,
            &backup_cfg.branch,
        )?;
        if let Err(e) = backup.seed_secret_cache(&mut secret_cache, &private_key_pem) {
            tracing::warn!(error = %e, "git backup: could not read committed secrets from the remote");
        }
        let files = cl_backup::tree::build(&cluster_state_for_build, &key, &recipient, &mut secret_cache);
        let author_ref = author_owned.as_ref().map(|(n, e)| (n.as_str(), e.as_str()));
        let oid = backup.reconcile(
            &files,
            "reconcile: mirror cluster state",
            &private_key_pem,
            author_ref,
        )?;
        Ok::<_, cl_backup::error::BackupError>((oid, secret_cache))
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;
    cache_state.secret_cache = secret_cache;

    cache_state.last_snapshot = Some(cl_backup::snapshot::Snapshot::from_state(&cluster_state));
    if let Err(e) = cache_state.save(&cache_path) {
        warn!(
            error = %e,
            "git backup: could not persist change-detection cache after reconcile"
        );
    }

    publish_last_export(
        state,
        &LastExport {
            at: unix_now(),
            summary: "reconcile: mirror cluster state".to_string(),
            commit: Some(commit_oid.to_string()),
            error: None,
        },
    )
    .await?;

    publish_last_pull(
        state,
        LastPull {
            at: unix_now(),
            commit: Some(commit_oid.to_string()),
            rejected: None,
        },
    )
    .await?;

    Ok(())
}

async fn publish_last_export(state: &AppState, export: &LastExport) -> Result<(), ApiError> {
    let content =
        cl_raft::state::base64_encode(&serde_json::to_vec(export).map_err(ApiError::internal)?);
    let response = state
        .cluster
        .submit(ClusterRequest::PutConfig {
            name: LAST_EXPORT_CONFIG.to_string(),
            content,
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Internal(format!(
            "publishing export status: {msg}"
        )));
    }
    Ok(())
}

enum PullDecision {
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

/// Public so `cl-daemon`'s `watch_git_backup` can pull before an export rebuilds the tracked tree
/// from raft (see its doc comment for the race).
pub async fn run_pull(state: &AppState) -> Result<(), ApiError> {
    run_pull_outcome(state).await.map(|_| ())
}

/// What one pull did.
enum PullOutcome {
    NoChange,
    Rejected(String),
    Applied,
}

async fn run_pull_outcome(state: &AppState) -> Result<PullOutcome, ApiError> {
    let backup_cfg = state
        .cluster_config
        .read()
        .await
        .backup
        .clone()
        .ok_or_else(|| ApiError::Conflict("git backup is not configured".into()))?;

    let cluster_state = state.cluster.snapshot().await;
    let key = {
        let key_guard = state.key.read().await;
        key_guard
            .as_ref()
            .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?
            .clone()
    };
    let private_key_pem =
        cl_backup::deploy_key::load_private(&cluster_state, &key).ok_or_else(|| {
            ApiError::Conflict(
                "no git deploy key yet - it is generated after the first export".into(),
            )
        })?;
    let existing_secrets: BTreeSet<String> = cluster_state.secrets.keys().cloned().collect();
    let existing_stacks: BTreeSet<String> = cluster_state.stacks.keys().cloned().collect();
    let last_good_oid =
        last_known_good_commit(&cluster_state).and_then(|s| git2::Oid::from_str(&s).ok());

    let dir = state.data_dir.join("git-backup");
    let decision = tokio::task::spawn_blocking(move || {
        pull_and_decide(
            &dir,
            &backup_cfg,
            &private_key_pem,
            &existing_secrets,
            &existing_stacks,
            last_good_oid,
        )
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;

    match decision {
        PullDecision::NoChange => Ok(PullOutcome::NoChange),
        PullDecision::Rejected {
            reason,
            preserved_branch,
        } => {
            let rejected_reason = reason.clone();
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
                    rejected: Some(RejectedPull {
                        reason,
                        preserved_branch,
                    }),
                },
            )
            .await?;
            Ok(PullOutcome::Rejected(rejected_reason))
        }
        PullDecision::Accepted {
            candidate,
            cluster_config,
            parsed,
        } => {
            apply_pulled_tree(state, &cluster_config, &parsed).await?;

            publish_last_pull(
                state,
                LastPull {
                    at: unix_now(),
                    commit: Some(candidate.to_string()),
                    rejected: None,
                },
            )
            .await?;
            Ok(PullOutcome::Applied)
        }
    }
}

async fn publish_last_pull(state: &AppState, last_pull: LastPull) -> Result<(), ApiError> {
    let content =
        cl_raft::state::base64_encode(&serde_json::to_vec(&last_pull).map_err(ApiError::internal)?);
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

/// Blocking git2 half: fetch, reject if the secrets subtree changed against the last known good
/// commit (git cannot supply new secrets), parse the candidate from the odb, validate.
fn pull_and_decide(
    dir: &Path,
    cfg: &cl_config::GitBackupConfig,
    private_key_pem: &str,
    existing_secrets: &BTreeSet<String>,
    existing_stacks: &BTreeSet<String>,
    last_good_oid: Option<git2::Oid>,
) -> cl_backup::Result<PullDecision> {
    let _repo_guard = cl_backup::git::lock_repo();
    let backup = cl_backup::git::GitBackup::open_or_init(dir, &cfg.repo_url, &cfg.branch)?;

    let Some(candidate) = backup.fetch_remote_tip(private_key_pem)? else {
        return Ok(PullDecision::NoChange);
    };

    if Some(candidate) == last_good_oid {
        return Ok(PullDecision::NoChange);
    }

    let candidate_commit = backup.repo().find_commit(candidate)?;

    if let Some(good_oid) = last_good_oid {
        if let Ok(good_commit) = backup.repo().find_commit(good_oid) {
            if good_commit.tree()?.id() == candidate_commit.tree()?.id() {
                return Ok(PullDecision::NoChange);
            }
        }
    }

    // With no known good commit the candidate's parent is the baseline, so a lost status entry
    // cannot make every commit look like a secret rotation.
    // The tofu state is export-only too: a changed .tfstate.age is rejected like a secret.
    if backup.tfstate_changed_against_baseline(last_good_oid, candidate)? {
        let reason = "the pulled commit changes a terraform/*/*.tfstate.age file; the tofu state is export-only \
                       and lives in the cluster - it cannot be edited through git"
            .to_string();
        let outcome =
            backup.reject_and_restore(candidate, last_good_oid, &reason, private_key_pem, None)?;
        return Ok(PullDecision::Rejected {
            reason,
            preserved_branch: outcome.preserved_branch,
        });
    }

    if backup.secrets_changed_against_baseline(last_good_oid, candidate)? {
        let reason = "the pulled commit changes something under secrets/, which this cluster cannot decrypt back \
                       into a live secret - secret rotation must go through the api, not git"
            .to_string();
        let outcome =
            backup.reject_and_restore(candidate, last_good_oid, &reason, private_key_pem, None)?;
        return Ok(PullDecision::Rejected {
            reason,
            preserved_branch: outcome.preserved_branch,
        });
    }

    let candidate_tree = candidate_commit.tree()?.id();
    let parsed = parse_tree(backup.repo(), candidate_tree)?;

    match validate_parsed(&parsed, existing_secrets, existing_stacks) {
        Ok(cluster_config) => Ok(PullDecision::Accepted {
            candidate,
            cluster_config: Box::new(cluster_config),
            parsed: Box::new(parsed),
        }),
        Err(reason) => {
            let outcome = backup.reject_and_restore(
                candidate,
                last_good_oid,
                &reason,
                private_key_pem,
                None,
            )?;
            Ok(PullDecision::Rejected {
                reason,
                preserved_branch: outcome.preserved_branch,
            })
        }
    }
}

/// Parse + validate only. A stack's `file_secrets` can only be satisfied by a secret the cluster
/// already holds. A raft-submission failure after this passes is not "malformed".
fn validate_parsed(
    parsed: &ParsedTree,
    existing_secrets: &BTreeSet<String>,
    existing_stacks: &BTreeSet<String>,
) -> Result<ClusterConfig, String> {
    let Some(yaml) = &parsed.cluster_yaml else {
        return Err("no cluster.yaml in the pulled tree".to_string());
    };
    let cluster_config = ClusterConfig::parse(yaml).map_err(|e| format!("cluster.yaml: {e}"))?;

    // `apply_pulled_tree` removes every stack the tree omits, so an empty tree (bad merge, `git rm -r
    // stacks/`, unrelated repo) would tear down all workloads. Emptying a cluster goes through the api.
    if parsed.stacks.is_empty() && !existing_stacks.is_empty() {
        return Err(format!(
            "the pulled tree has no stacks/ at all, which would remove all {} stack(s) \
             this cluster runs - remove stacks through the api, not by emptying the repo",
            existing_stacks.len()
        ));
    }

    for (name, spec) in &parsed.stacks {
        // A stack already running under this name stays pullable (as `deploy_from_files`).
        if !existing_stacks.contains(name) {
            validate_stack_name(name).map_err(|e| format!("stacks/{name}.yaml: {e}"))?;
        }
        let stack = StackFile::parse(spec).map_err(|e| format!("stacks/{name}.yaml: {e}"))?;
        stack
            .validate_named(name, &cluster_config)
            .map_err(|e| format!("stacks/{name}.yaml: {e}"))?;

        let requirements = stack.requirements();
        for config_name in &requirements.file_configs {
            if !parsed.configs.contains_key(config_name) {
                return Err(format!(
                    "stacks/{name}.yaml: config {config_name} has no content under configs/"
                ));
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
    for (stack_name, modules) in &parsed.terraform {
        let Some(spec) = parsed.stacks.get(stack_name) else {
            return Err(format!(
                "terraform/{stack_name}/: no stack {stack_name} in stacks/"
            ));
        };
        let stack = StackFile::parse(spec).map_err(|e| format!("stacks/{stack_name}.yaml: {e}"))?;
        for (service, source) in modules {
            if !stack.services.contains_key(service) {
                return Err(format!(
                    "terraform/{stack_name}/{service}.tf: stack {stack_name} has no service {service}"
                ));
            }
            cl_config::validate_module(source)
                .map_err(|e| format!("terraform/{stack_name}/{service}.tf: {e}"))?;
        }
    }
    Ok(cluster_config)
}

async fn apply_pulled_tree(
    state: &AppState,
    cluster_config: &ClusterConfig,
    parsed: &ParsedTree,
) -> Result<(), ApiError> {
    let current = state.cluster.snapshot().await;
    let yaml_changed =
        current.cluster_config.as_ref().map(|c| c.yaml.as_str()) != parsed.cluster_yaml.as_deref();
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
        // `DeployStack` always bumps the revision, which feeds every instance's `clouduler.revision`
        // label (`cl_sched::spec::build`); calling it unconditionally would recreate every container on
        // every pull. Skip a stack whose spec is unchanged.
        if current.stacks.get(name).is_some_and(|s| &s.spec == spec) {
            continue;
        }
        deploy_from_files(state, name, spec.clone(), configs).await?;
    }

    let current = state.cluster.snapshot().await;
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

    apply_pulled_terraform(state, parsed).await?;

    for (name, bytes) in &parsed.configs {
        // Reserved configs are mirrored into the export but each has its own validated write path;
        // a raw `PutConfig` of git content would bypass it.
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

    let current = state.cluster.snapshot().await;
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

/// Applies the tree's `.tf` modules, never the state (export-only). A stack whose module set
/// already matches raft is skipped, so this never touches an unchanged stack, and it is
/// independent of the spec redeploy: editing a module does not restart containers. A stack
/// with modules in raft but no `terraform/<stack>/` in the tree gets an empty set.
async fn apply_pulled_terraform(state: &AppState, parsed: &ParsedTree) -> Result<(), ApiError> {
    let current = state.cluster.snapshot().await;
    let mut stacks: BTreeSet<&String> = parsed.terraform.keys().collect();
    stacks.extend(current.terraform_modules.keys());
    for stack in stacks {
        if !current.stacks.contains_key(stack.as_str()) {
            continue;
        }
        let desired: BTreeMap<String, String> =
            parsed.terraform.get(stack).cloned().unwrap_or_default();
        let existing: BTreeMap<String, String> = current
            .terraform_modules
            .get(stack)
            .map(|m| {
                m.iter()
                    .map(|(service, module)| (service.clone(), module.source.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if desired == existing {
            continue;
        }
        let response = state
            .cluster
            .submit(ClusterRequest::SetStackTerraform {
                stack: stack.clone(),
                modules: desired,
            })
            .await
            .map_err(ApiError::not_leader)?;
        if let Some(msg) = response.error {
            return Err(ApiError::Internal(format!(
                "writing terraform modules of {stack}: {msg}"
            )));
        }
    }
    Ok(())
}

/// Held for a whole rollback and around each export pass of the daemon's export loop, so the
/// loop cannot rebuild the tree from stale raft state on top of the rollback commit before the
/// pull has applied it.
pub fn export_guard() -> &'static tokio::sync::Mutex<()> {
    static GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    &GUARD
}

const COMMIT_LIST_LIMIT: usize = 50;

async fn git_credentials(
    state: &AppState,
) -> Result<(cl_config::GitBackupConfig, String), ApiError> {
    let backup_cfg = state
        .cluster_config
        .read()
        .await
        .backup
        .clone()
        .ok_or_else(|| ApiError::Conflict("git backup is not configured".into()))?;
    let cluster_state = state.cluster.snapshot().await;
    let key_guard = state.key.read().await;
    let key = key_guard
        .as_ref()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;
    let pem = cl_backup::deploy_key::load_private(&cluster_state, key).ok_or_else(|| {
        ApiError::Conflict("no git deploy key yet - it is generated after the first export".into())
    })?;
    Ok((backup_cfg, pem))
}

fn backup_error(e: cl_backup::BackupError) -> ApiError {
    match e {
        cl_backup::BackupError::BadCommit(m) => ApiError::BadRequest(m),
        cl_backup::BackupError::PushRejected(m) => ApiError::Conflict(format!(
            "the remote moved or refused the push, try again: {m}"
        )),
        other => ApiError::internal(other),
    }
}

/// `GET /api/git-backup/commits`: the newest commits of the tracked branch, fetched first.
async fn list_commits(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> Result<Json<Vec<CommitInfo>>, ApiError> {
    commits_of(&state).await.map(Json)
}

/// The cli's copy of `list_commits` (internal auth).
async fn internal_commits(
    _auth: InternalAuth,
    State(state): State<AppState>,
) -> Result<Json<Vec<CommitInfo>>, ApiError> {
    commits_of(&state).await.map(Json)
}

async fn commits_of(state: &AppState) -> Result<Vec<CommitInfo>, ApiError> {
    let (cfg, pem) = git_credentials(state).await?;
    let dir = state.data_dir.join("git-backup");
    tokio::task::spawn_blocking(move || {
        let _guard = cl_backup::git::lock_repo();
        cl_backup::git::GitBackup::open_or_init(&dir, &cfg.repo_url, &cfg.branch)?
            .list_commits(&pem, COMMIT_LIST_LIMIT)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(backup_error)
}

#[derive(Deserialize, Serialize)]
struct RollbackRequest {
    commit: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RollbackResponse {
    commit: String,
}

/// `POST /api/git-backup/rollback`: leader-only, so a non-leader forwards.
async fn rollback_admin(
    admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<RollbackRequest>,
) -> Result<Json<RollbackResponse>, ApiError> {
    let author = (
        admin.0.username.clone(),
        format!("{}@localhost", admin.0.username),
    );
    if state.cluster.is_leader() {
        return run_rollback(&state, &body.commit, author).await.map(Json);
    }
    let leader = leader_node_name(&state)
        .await
        .ok_or_else(|| ApiError::Unavailable("no cluster leader known right now".into()))?;
    let query = serde_urlencoded::to_string([
        ("commit", body.commit.as_str()),
        ("author", admin.0.username.as_str()),
    ])
    .unwrap_or_default();
    let path = format!("/api/git-backup/internal-rollback?{query}");
    internal_peer::post_json_relay(&state, &leader, &path)
        .await
        .map(Json)
}

async fn internal_rollback(
    _auth: InternalAuth,
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<RollbackResponse>, ApiError> {
    let commit = params
        .get("commit")
        .ok_or_else(|| ApiError::BadRequest("missing commit".into()))?;
    let username = params
        .get("author")
        .cloned()
        .unwrap_or_else(|| "clouduler".into());
    let author = (username.clone(), format!("{username}@localhost"));
    run_rollback(&state, commit, author).await.map(Json)
}

/// Folds in any remote-only commit, pushes the revert commit on the tip, then applies it through
/// the normal pull path. A rejection by that path is a 409 with its reason; the tracked branch has
/// been restored by then and the revert commit lives on a `rejected/` branch.
async fn run_rollback(
    state: &AppState,
    commit: &str,
    author: (String, String),
) -> Result<RollbackResponse, ApiError> {
    let (cfg, pem) = git_credentials(state).await?;
    let _guard = export_guard().lock().await;

    if let PullOutcome::Rejected(reason) = run_pull_outcome(state).await? {
        warn!(%reason, "git backup: a pending remote commit was rejected before the rollback");
    }

    let dir = state.data_dir.join("git-backup");
    let spec = commit.to_string();
    let new_head = tokio::task::spawn_blocking(move || {
        let _repo_guard = cl_backup::git::lock_repo();
        let backup = cl_backup::git::GitBackup::open_or_init(&dir, &cfg.repo_url, &cfg.branch)?;
        backup.rollback(&spec, &pem, Some((author.0.as_str(), author.1.as_str())))
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(backup_error)?;
    let Some(new_head) = new_head else {
        return Err(ApiError::Conflict(
            "the configuration already matches that commit (secrets and terraform state are never rolled back)".into(),
        ));
    };

    match run_pull_outcome(state).await? {
        PullOutcome::Rejected(reason) => Err(ApiError::Conflict(format!(
            "the cluster rejected the rolled back configuration: {reason}"
        ))),
        _ => Ok(RollbackResponse {
            commit: new_head.to_string(),
        }),
    }
}

#[derive(Deserialize, Serialize)]
struct ResetRequest {
    commit: String,
    #[serde(default)]
    release_tag: Option<String>,
    #[serde(default)]
    keep_current_release: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct ResetResponse {
    commit: String,
    branch: String,
    /// The release the nodes download; `None` keeps the running one.
    release_tag: Option<String>,
    /// Where `release_tag` came from: "explicit", "commit" or "current".
    release_source: String,
}

/// `POST /api/git-backup/reset`: leader-only, so a non-leader forwards. 202: the work continues
/// on every node (see `cl_update::reset`).
async fn reset_admin(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<ResetRequest>,
) -> Result<(StatusCode, Json<ResetResponse>), ApiError> {
    if state.cluster.is_leader() {
        let done = run_reset(
            &state,
            &body.commit,
            body.release_tag.as_deref(),
            body.keep_current_release,
        )
        .await?;
        return Ok((StatusCode::ACCEPTED, Json(done)));
    }
    let leader = leader_node_name(&state)
        .await
        .ok_or_else(|| ApiError::Unavailable("no cluster leader known right now".into()))?;
    let mut query = vec![("commit", body.commit.as_str())];
    if let Some(tag) = body.release_tag.as_deref() {
        query.push(("release_tag", tag));
    }
    if body.keep_current_release {
        query.push(("keep_current_release", "true"));
    }
    let path = format!(
        "/api/git-backup/internal-reset?{}",
        serde_urlencoded::to_string(query).unwrap_or_default()
    );
    let done = internal_peer::post_json_relay(&state, &leader, &path).await?;
    Ok((StatusCode::ACCEPTED, Json(done)))
}

async fn internal_reset(
    _auth: InternalAuth,
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<(StatusCode, Json<ResetResponse>), ApiError> {
    let commit = params
        .get("commit")
        .ok_or_else(|| ApiError::BadRequest("missing commit".into()))?;
    let done = run_reset(
        &state,
        commit,
        params.get("release_tag").map(String::as_str),
        params
            .get("keep_current_release")
            .is_some_and(|v| v == "true"),
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(done)))
}

/// The distinct arches of the active nodes: each needs an asset in the release.
fn node_arches(state: &cl_raft::ClusterState) -> std::collections::BTreeSet<String> {
    state
        .nodes
        .values()
        .filter(|n| n.availability == cl_raft::state::Availability::Active)
        .map(|n| n.arch.clone())
        .collect()
}

/// The 400 for a release the nodes could not download. Nothing was changed yet.
fn release_unusable(tag: &str, source: &str, reason: &str) -> ApiError {
    let hint = if source == "commit" {
        "the tag comes from .clouduler-version.json at that commit; set an explicit release_tag or keep the running release"
    } else {
        "pick another release_tag or keep the running release"
    };
    ApiError::BadRequest(format!(
        "release {tag} cannot be used for the reset ({reason}); {hint}. Nothing was changed."
    ))
}

/// Same release, whatever the `v` prefix.
fn same_release(tag: &str, version: &str) -> bool {
    tag.trim_start_matches('v') == version.trim_start_matches('v')
}

/// Resets the cluster to the configuration of `commit`: moves the remote branch back (the old tip
/// goes to a `rolled-back/` branch), then hands the cluster a plan that every node carries out
/// together once they all hold the release recorded at that commit (or `release_override`). No pull
/// checks run: secrets, terraform state and the rest of raft are not touched. What is still
/// checked is only what would leave a node unable to start: the commit's cluster.yaml and stacks
/// must parse and must still declare every node the cluster has.
async fn run_reset(
    state: &AppState,
    commit: &str,
    release_override: Option<&str>,
    keep_current_release: bool,
) -> Result<ResetResponse, ApiError> {
    let override_tag = match release_override.map(str::trim).filter(|t| !t.is_empty()) {
        Some(tag) if tag.contains('/') || tag.contains(char::is_whitespace) => {
            return Err(ApiError::BadRequest(
                "release_tag must have no '/' or whitespace".into(),
            ))
        }
        other => other.map(str::to_string),
    };
    if keep_current_release && override_tag.is_some() {
        return Err(ApiError::BadRequest(
            "keep_current_release and release_tag are mutually exclusive".into(),
        ));
    }
    let cluster_state = state.cluster.snapshot().await;
    if cluster_state.in_emergency() {
        return Err(ApiError::Conflict(
            "the cluster is in emergency mode; end it before resetting the configuration".into(),
        ));
    }
    if cl_update::reset::in_flight(&cluster_state) {
        return Err(ApiError::Conflict(
            "a configuration reset is already in progress".into(),
        ));
    }
    if cl_update::orchestrator::load_run(&cluster_state).is_some_and(|run| run.in_progress()) {
        return Err(ApiError::Conflict(
            "a self-update run is in progress; wait for it or cancel it".into(),
        ));
    }
    let (cfg, pem) = git_credentials(state).await?;
    let _guard = export_guard().lock().await;

    let dir = state.data_dir.join("git-backup");
    let spec = commit.to_string();
    let (target, parsed, pem) = {
        let pem = pem.clone();
        tokio::task::spawn_blocking(move || {
            let _repo_guard = cl_backup::git::lock_repo();
            let backup = cl_backup::git::GitBackup::open_or_init(&dir, &cfg.repo_url, &cfg.branch)?;
            let target = backup.prepare_reset(&spec, &pem)?;
            let tree = backup.repo().find_commit(target.target)?.tree()?.id();
            let parsed = parse_tree(backup.repo(), tree)?;
            Ok::<_, cl_backup::BackupError>((target, parsed, pem))
        })
        .await
        .map_err(ApiError::internal)?
        .map_err(backup_error)?
    };

    let release_source = if keep_current_release {
        "current"
    } else if override_tag.is_some() {
        "explicit"
    } else if target.version.is_some() {
        "commit"
    } else {
        "current"
    };
    let release_tag = override_tag
        .or_else(|| target.version.clone())
        .filter(|_| !keep_current_release)
        .filter(|tag| !same_release(tag, cl_config::version()));
    if release_tag.is_some() && state.cluster_config.read().await.self_update.is_none() {
        return Err(ApiError::Conflict(
            "that commit was exported by another release but no self-update release is configured to download it".into(),
        ));
    }

    // Resolve the release the way the hold-mode run will, before anything is pushed.
    if let Some(tag) = release_tag.as_deref() {
        let self_update = state.cluster_config.read().await.self_update.clone();
        if let Some(self_update) = self_update {
            let arches = node_arches(&cluster_state);
            let token = super::self_update::unseal_github_token(state).await;
            cl_update::release::validate_pinned(
                &self_update.release_url,
                tag,
                token.as_deref(),
                arches.iter().map(String::as_str),
            )
            .await
            .map_err(|e| release_unusable(tag, release_source, &e))?;
        }
    }

    let now = super::self_update::now();
    let content = build_reset_content(&parsed, &cluster_state, now).map_err(ApiError::Conflict)?;
    let id = format!("reset-{now}");
    let run_id = release_tag.as_ref().map(|_| format!("run-{id}"));
    let mut plan = cl_update::reset::ResetPlan {
        id,
        commit: target.target.to_string(),
        branch: String::new(),
        release_tag: release_tag.clone(),
        run_id: run_id.clone(),
        started_at: now,
        phase: cl_update::reset::ResetPhase::Preparing,
        content,
    };
    // The plan rides a config object, which raft caps.
    let encoded_len = plan.encode().map_err(ApiError::Internal)?.len();
    if encoded_len > cl_raft::MAX_OBJECT_BYTES {
        return Err(ApiError::Conflict(format!(
            "the configuration at that commit is too large to hand to every node ({encoded_len} bytes, limit {})",
            cl_raft::MAX_OBJECT_BYTES
        )));
    }

    // The point of no return: the remote is rewritten. Everything before only read.
    let dir = state.data_dir.join("git-backup");
    let cfg = state
        .cluster_config
        .read()
        .await
        .backup
        .clone()
        .ok_or_else(|| ApiError::Conflict("git backup is not configured".into()))?;
    let pushed = target.clone();
    let branch = tokio::task::spawn_blocking(move || {
        let _repo_guard = cl_backup::git::lock_repo();
        cl_backup::git::GitBackup::open_or_init(&dir, &cfg.repo_url, &cfg.branch)?
            .push_reset(&pushed, now, &pem)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(backup_error)?;
    plan.branch = branch.clone();

    // The target is the last known good commit, so the pull path sees the force push as nothing new.
    publish_last_pull(
        state,
        LastPull {
            at: unix_now(),
            commit: Some(target.target.to_string()),
            rejected: None,
        },
    )
    .await?;

    let revision = cl_update::reset::load_revision(&cluster_state)
        .map(|(_, r)| r)
        .unwrap_or(0);
    if release_tag.is_none() {
        plan.phase = cl_update::reset::ResetPlan::go_phase(&state.cluster, now);
    }
    store_reset_plan(state, &plan, revision).await?;

    if let (Some(tag), Some(run_id)) = (release_tag.clone(), run_id) {
        let started = start_reset_run(state, run_id, tag).await;
        if let Err(e) = started {
            plan.phase = cl_update::reset::ResetPhase::Failed {
                reason: format!("could not start the release download: {e}"),
            };
            let latest = cl_update::reset::load_revision(&*state.cluster.snapshot().await)
                .map(|(_, r)| r)
                .unwrap_or(0);
            let _ = store_reset_plan(state, &plan, latest).await;
            return Err(e);
        }
    }

    Ok(ResetResponse {
        commit: target.target.to_string(),
        branch,
        release_tag,
        release_source: release_source.into(),
    })
}

async fn store_reset_plan(
    state: &AppState,
    plan: &cl_update::reset::ResetPlan,
    revision: u64,
) -> Result<(), ApiError> {
    let request = plan.put_request(revision).map_err(ApiError::Internal)?;
    let response = state
        .cluster
        .submit(request)
        .await
        .map_err(ApiError::not_leader)?;
    match response.error {
        Some(msg) => Err(ApiError::Conflict(msg)),
        None => Ok(()),
    }
}

/// The hold-mode self-update run that makes every node download `tag`.
async fn start_reset_run(state: &AppState, id: String, tag: String) -> Result<(), ApiError> {
    let cluster_state = state.cluster.snapshot().await;
    let revision = cl_update::orchestrator::load_run_revision(&cluster_state)
        .map(|(_, r)| r)
        .unwrap_or(0);
    let nodes = cluster_state
        .nodes
        .iter()
        .filter(|(_, n)| n.availability == cl_raft::state::Availability::Active)
        .map(|(name, _)| name.clone());
    let mut run = cl_update::plan::UpdateRunState::new(id, nodes, super::self_update::now());
    run.target_tag = Some(tag);
    run.hold = true;
    super::self_update::submit_run(state, &run, revision).await
}

/// What every node will write: the commit's cluster.yaml, stacks and plain configs. The only
/// checks are the ones that keep a node from starting: parseable yaml, and every node of the
/// cluster still declared (a node missing from the file wipes itself on its next start).
fn build_reset_content(
    parsed: &ParsedTree,
    current: &cl_raft::ClusterState,
    now: i64,
) -> Result<cl_raft::store::ResetContent, String> {
    let yaml = parsed
        .cluster_yaml
        .clone()
        .ok_or("that commit has no cluster.yaml")?;
    let config =
        ClusterConfig::parse(&yaml).map_err(|e| format!("cluster.yaml at that commit: {e}"))?;
    let missing: Vec<&str> = current
        .nodes
        .iter()
        .filter(|(name, node)| {
            node.availability == cl_raft::state::Availability::Active
                && !config.nodes.contains_key(name.as_str())
        })
        .map(|(name, _)| name.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "cluster.yaml at that commit does not declare node(s) {}, which would leave them unable to start; \
             a reset cannot go back to before they joined",
            missing.join(", ")
        ));
    }

    let mut owners: BTreeMap<String, String> = BTreeMap::new();
    let mut stacks = Vec::new();
    for (name, spec) in &parsed.stacks {
        let file = StackFile::parse(spec).map_err(|e| format!("stacks/{name}.yaml: {e}"))?;
        let requirements = file.requirements();
        for config in &requirements.file_configs {
            owners.insert(config.clone(), name.clone());
        }
        stacks.push(cl_raft::store::ResetStack {
            name: name.clone(),
            spec: spec.clone(),
            configs: requirements.file_configs,
            secrets: requirements.file_secrets,
            used_configs: requirements.configs,
            used_secrets: requirements.secrets,
        });
    }

    let kept = |name: &str| {
        !cl_backup::snapshot::is_bookkeeping_config(name) && !super::objects::is_reserved(name)
    };
    let configs = parsed
        .configs
        .iter()
        .filter(|(name, _)| kept(name))
        .map(|(name, bytes)| cl_raft::store::ResetConfig {
            name: name.clone(),
            content: cl_backup::tree::encode_config_content(bytes),
            owner: owners.get(name).cloned(),
        })
        .collect();
    let remove_configs = current
        .configs
        .keys()
        .filter(|name| kept(name) && !parsed.configs.contains_key(name.as_str()))
        .cloned()
        .collect();

    Ok(cl_raft::store::ResetContent {
        cluster_yaml: Some(yaml),
        generation: current
            .cluster_config
            .as_ref()
            .map(|c| c.generation)
            .unwrap_or(0)
            + 1,
        at: now,
        stacks,
        configs,
        remove_configs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::harness;

    const SPEC: &str = "services:\n  web:\n    image: nginx\n";
    const SPEC_V2: &str = "services:\n  web:\n    image: nginx:1.27\n";

    fn tree(stacks: &[(&str, &str)]) -> ParsedTree {
        ParsedTree {
            cluster_yaml: None,
            stacks: stacks
                .iter()
                .map(|(n, s)| (n.to_string(), s.to_string()))
                .collect(),
            configs: BTreeMap::new(),
            secrets: BTreeMap::new(),
            terraform: BTreeMap::new(),
            terraform_states: BTreeMap::new(),
        }
    }

    /// Regression: every stack in a pulled tree was redeployed (bumping `clouduler.revision` and
    /// recreating containers) even when its spec was unchanged.
    #[tokio::test]
    async fn a_pull_does_not_redeploy_an_unchanged_stack() {
        let h = harness().await;
        h.state
            .cluster
            .submit(ClusterRequest::DeployStack {
                name: "site".to_string(),
                spec: SPEC.to_string(),
                at: 0,
            })
            .await
            .expect("deploy");
        let before = h.state.cluster.snapshot().await;
        assert_eq!(before.stacks["site"].revision, 1);

        let cluster_config = h.state.cluster_config.read().await.clone();
        apply_pulled_tree(&h.state, &cluster_config, &tree(&[("site", SPEC)]))
            .await
            .expect("apply unchanged tree");

        let after = h.state.cluster.snapshot().await;
        assert_eq!(
            after.stacks["site"].revision, 1,
            "an unchanged spec must not bump the stack's revision"
        );
    }

    const CLUSTER_YAML: &str = include_str!("../../../../examples/cluster.yaml");

    #[tokio::test]
    async fn commits_and_rollback_need_git_backup_configured() {
        let h = harness().await;
        let err = commits_of(&h.state).await.expect_err("not configured");
        assert!(matches!(err, ApiError::Conflict(m) if m == "git backup is not configured"));
        let err = run_rollback(&h.state, "abcd1234", ("a".into(), "a@localhost".into()))
            .await
            .expect_err("not configured");
        assert!(matches!(err, ApiError::Conflict(m) if m == "git backup is not configured"));
    }

    #[test]
    fn unusable_release_message_guides_by_source() {
        let ApiError::BadRequest(m) = release_unusable("v1", "commit", "no asset") else {
            panic!("expected 400")
        };
        assert!(m.contains("keep the running release") && m.contains("explicit release_tag"));
        assert!(m.contains("Nothing was changed"));
        let ApiError::BadRequest(m) = release_unusable("v1", "explicit", "not found") else {
            panic!("expected 400")
        };
        assert!(m.contains("pick another release_tag"));
    }

    #[tokio::test]
    async fn a_reset_is_refused_while_another_is_in_flight_or_a_run_is_going() {
        let h = harness().await;
        let err = run_reset(&h.state, "abcd1234", None, false)
            .await
            .expect_err("not configured");
        assert!(matches!(err, ApiError::Conflict(m) if m == "git backup is not configured"));

        let mut plan = cl_update::reset::ResetPlan {
            id: "reset-1".into(),
            commit: "a".repeat(40),
            branch: "rolled-back/x".into(),
            release_tag: None,
            run_id: None,
            started_at: 0,
            phase: cl_update::reset::ResetPhase::Preparing,
            content: cl_raft::store::ResetContent {
                cluster_yaml: None,
                generation: 1,
                at: 0,
                stacks: vec![],
                configs: vec![],
                remove_configs: vec![],
            },
        };
        store_reset_plan(&h.state, &plan, 0)
            .await
            .expect("store plan");
        let err = run_reset(&h.state, "abcd1234", None, false)
            .await
            .expect_err("in flight");
        assert!(matches!(err, ApiError::Conflict(m) if m.contains("already in progress")));

        // A finished plan no longer blocks (the next check is the git one).
        plan.phase = cl_update::reset::ResetPhase::Done;
        store_reset_plan(&h.state, &plan, 1).await.expect("done");
        let err = run_reset(&h.state, "abcd1234", None, false)
            .await
            .expect_err("not configured");
        assert!(matches!(err, ApiError::Conflict(m) if m == "git backup is not configured"));

        let err = run_reset(&h.state, "abcd1234", Some("v1/2"), false)
            .await
            .expect_err("bad tag");
        assert!(matches!(err, ApiError::BadRequest(_)));

        let err = run_reset(&h.state, "abcd1234", Some("v1.0.0"), true)
            .await
            .expect_err("both given");
        assert!(matches!(err, ApiError::BadRequest(m) if m.contains("mutually exclusive")));
    }

    #[test]
    fn backup_errors_map_to_400_and_409() {
        assert!(matches!(
            backup_error(cl_backup::BackupError::BadCommit("no".into())),
            ApiError::BadRequest(_)
        ));
        assert!(matches!(
            backup_error(cl_backup::BackupError::PushRejected("moved".into())),
            ApiError::Conflict(_)
        ));
    }

    #[test]
    fn the_reset_content_comes_from_the_tree_without_touching_secrets() {
        let mut parsed = tree(&[("site", "services:\n  web:\n    image: nginx\n")]);
        parsed.cluster_yaml = Some(CLUSTER_YAML.to_string());
        parsed.configs.insert("kept.conf".into(), b"a".to_vec());
        parsed
            .secrets
            .insert("db".into(), b"age-ciphertext".to_vec());
        // Reserved bookkeeping that happens to be in the tree is never written back.
        parsed
            .configs
            .insert(cl_backup::status::LAST_PULL_CONFIG.into(), b"{}".to_vec());

        let mut current = cl_raft::ClusterState {
            cluster_config: Some(cl_raft::state::ClusterConfigEntry {
                yaml: "x".into(),
                generation: 7,
            }),
            ..Default::default()
        };
        for name in ["stale.conf", cl_backup::status::LAST_PULL_CONFIG] {
            current.configs.insert(
                name.into(),
                cl_raft::state::ConfigEntry {
                    name: name.into(),
                    content: String::new(),
                    revision: 1,
                    owner: None,
                },
            );
        }

        let content = build_reset_content(&parsed, &current, 50).expect("content");
        assert_eq!(content.generation, 8);
        assert_eq!(content.at, 50);
        assert_eq!(content.stacks.len(), 1);
        assert_eq!(content.stacks[0].name, "site");
        let names: Vec<&str> = content.configs.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["kept.conf"]);
        assert_eq!(content.remove_configs, vec!["stale.conf".to_string()]);

        // No cluster.yaml, or one that drops a node of the cluster, is refused.
        let mut no_yaml = tree(&[]);
        no_yaml.cluster_yaml = None;
        assert!(build_reset_content(&no_yaml, &current, 0).is_err());
        let mut with_node = cl_raft::ClusterState::default();
        with_node.nodes.insert(
            "ghost".into(),
            cl_raft::state::NodeState {
                name: "ghost".into(),
                id: uuid::Uuid::new_v4(),
                role: cl_raft::state::Role::Worker,
                rpc_addr: "10.0.0.9:7373".parse().unwrap(),
                drives: Default::default(),
                availability: cl_raft::state::Availability::Active,
                labels: Default::default(),
                last_resort: false,
                egress_networks: Default::default(),
                networks: Default::default(),
                open_ports: Default::default(),
                open_all_ports: Default::default(),
                arch: String::new(),
                binary_hash: String::new(),
                ui_hash: Default::default(),
                started_at: Default::default(),
                node_pubkey: None,
                memory_total_bytes: 0,
                gpus: Default::default(),
                images: Default::default(),
                version: Default::default(),
            },
        );
        let err = build_reset_content(&parsed, &with_node, 0).expect_err("ghost node");
        assert!(err.contains("ghost"), "{err}");
    }

    #[test]
    fn a_tree_with_no_stacks_is_rejected_when_the_cluster_has_some() {
        let mut parsed = tree(&[]);
        parsed.cluster_yaml = Some(CLUSTER_YAML.to_string());

        let reason = validate_parsed(
            &parsed,
            &BTreeSet::new(),
            &["a", "b", "c"].map(String::from).into(),
        )
        .expect_err("an empty-stacks tree must be rejected");
        assert!(reason.contains("no stacks"), "unexpected reason: {reason}");

        assert!(validate_parsed(&parsed, &BTreeSet::new(), &BTreeSet::new()).is_ok());
    }

    #[tokio::test]
    async fn a_pull_redeploys_a_changed_stack() {
        let h = harness().await;
        h.state
            .cluster
            .submit(ClusterRequest::DeployStack {
                name: "site".to_string(),
                spec: SPEC.to_string(),
                at: 0,
            })
            .await
            .expect("deploy");

        let cluster_config = h.state.cluster_config.read().await.clone();
        apply_pulled_tree(&h.state, &cluster_config, &tree(&[("site", SPEC_V2)]))
            .await
            .expect("apply changed tree");

        let after = h.state.cluster.snapshot().await;
        assert_eq!(after.stacks["site"].revision, 2);
        assert_eq!(after.stacks["site"].spec, SPEC_V2);
    }

    const SPEC_TWO: &str = "services:\n  web:\n    image: nginx\n  db:\n    image: pg\n";
    const MODULE: &str = "variable \"x\" {}\n";

    fn with_modules(mut t: ParsedTree, stack: &str, mods: &[(&str, &str)]) -> ParsedTree {
        t.terraform.insert(
            stack.to_string(),
            mods.iter()
                .map(|(s, m)| (s.to_string(), m.to_string()))
                .collect(),
        );
        t
    }

    fn valid_tree() -> ParsedTree {
        let mut t = tree(&[("site", SPEC)]);
        t.cluster_yaml = Some(CLUSTER_YAML.to_string());
        t
    }

    #[test]
    fn validation_rejects_bad_terraform_modules() {
        let none = BTreeSet::new();
        let ok = with_modules(valid_tree(), "site", &[("web", MODULE)]);
        assert!(validate_parsed(&ok, &none, &none).is_ok());

        let unknown_service = with_modules(valid_tree(), "site", &[("nope", MODULE)]);
        let e = validate_parsed(&unknown_service, &none, &none).unwrap_err();
        assert!(e.contains("no service nope"), "{e}");

        let unknown_stack = with_modules(valid_tree(), "ghost", &[("web", MODULE)]);
        let e = validate_parsed(&unknown_stack, &none, &none).unwrap_err();
        assert!(e.contains("no stack ghost"), "{e}");

        let bad_hcl = with_modules(valid_tree(), "site", &[("web", "variable \"x\" {")]);
        let e = validate_parsed(&bad_hcl, &none, &none).unwrap_err();
        assert!(e.contains("invalid HCL"), "{e}");

        let backend = with_modules(
            valid_tree(),
            "site",
            &[("web", "terraform {\n  backend \"s3\" {}\n}\n")],
        );
        let e = validate_parsed(&backend, &none, &none).unwrap_err();
        assert!(e.contains("backend"), "{e}");
    }

    /// A `.tf`-only change lands via SetStackTerraform without redeploying the stack, and a stack
    /// whose `terraform/` dir vanished loses its modules.
    #[tokio::test]
    async fn a_pull_applies_modules_without_redeploying() {
        let h = harness().await;
        h.state
            .cluster
            .submit(ClusterRequest::DeployStack {
                name: "site".to_string(),
                spec: SPEC_TWO.to_string(),
                at: 0,
            })
            .await
            .expect("deploy");
        let cluster_config = h.state.cluster_config.read().await.clone();

        let t = with_modules(tree(&[("site", SPEC_TWO)]), "site", &[("web", MODULE)]);
        apply_pulled_tree(&h.state, &cluster_config, &t)
            .await
            .expect("apply");
        let st = h.state.cluster.snapshot().await;
        assert_eq!(st.stacks["site"].revision, 1, "no redeploy");
        assert_eq!(st.terraform_modules["site"]["web"].source, MODULE);
        let rev = st.terraform_modules["site"]["web"].revision;

        // Same tree again: untouched.
        apply_pulled_tree(&h.state, &cluster_config, &t)
            .await
            .expect("reapply");
        let st = h.state.cluster.snapshot().await;
        assert_eq!(st.terraform_modules["site"]["web"].revision, rev);

        // Edit one module, add another.
        let t = with_modules(
            tree(&[("site", SPEC_TWO)]),
            "site",
            &[("web", "variable \"y\" {}\n"), ("db", MODULE)],
        );
        apply_pulled_tree(&h.state, &cluster_config, &t)
            .await
            .expect("apply edit");
        let st = h.state.cluster.snapshot().await;
        assert_eq!(st.stacks["site"].revision, 1, "still no redeploy");
        assert_eq!(st.terraform_modules["site"].len(), 2);
        assert_eq!(st.terraform_modules["site"]["web"].revision, rev + 1);

        // terraform/site/ gone from the tree.
        apply_pulled_tree(&h.state, &cluster_config, &tree(&[("site", SPEC_TWO)]))
            .await
            .expect("apply removal");
        let st = h.state.cluster.snapshot().await;
        assert!(st.terraform_modules.is_empty());
        assert_eq!(st.stacks["site"].revision, 1);
    }
}
