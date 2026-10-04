//! `GET /api/git-backup/status`: read-only view of the git backup feature.
//!
//! Also `POST /api/git-backup/webhook/{token}`: the reverse direction. A git
//! host notifies this endpoint after a push; the leader fetches the tracked
//! branch, and if the new tip parses and validates cleanly, applies it to the
//! cluster (cluster config, stacks, and the plain configs they reference -
//! never secrets, see `validate_parsed` below). A commit that fails
//! validation is never applied: it is preserved on its own `rejected/...`
//! branch and the tracked branch is force-pushed back to the current cluster
//! state, so the remote is never left pointing at something this cluster
//! refuses to run.
//!
//! The token lives in the URL path rather than a header: some git hosts (and
//! most generic "webhook" integrations) offer no way to attach a custom
//! header, but every one of them lets you paste an arbitrary url, so an
//! unguessable path segment is the auth mechanism that actually works
//! everywhere.
//!
//! `clouduler git-backup pull` reaches `POST /api/git-backup/pull`: an
//! internal-auth route (the same HMAC-over-cluster-key scheme as every other
//! node-to-node call) rather than the token-in-url one, since the cli
//! already holds the cluster key on disk.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_backup::deploy_key::{
    public_from_state, DEPLOY_KEY_PRIVATE_SECRET, DEPLOY_KEY_PUBLIC_CONFIG,
};
use cl_backup::git::parse_tree;
use cl_backup::status::{
    last_known_good_commit, load as load_last_export, load_pull, LastExport, LastPull,
    RejectedPull, LAST_EXPORT_CONFIG, LAST_PULL_CONFIG,
};
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
        .route("/api/git-backup/pull", post(pull_now))
        .route("/api/git-backup/reconcile", post(reconcile_now))
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

async fn webhook_secret_status(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<WebhookSecretStatus> {
    let cluster_state = state.cluster.state().await;
    Json(WebhookSecretStatus {
        configured: cluster_state.secrets.contains_key(WEBHOOK_SECRET),
    })
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
/// `internal_peer` uses for every other node-to-node call.
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

/// Any node can run a pull once asked - the actual git work happens on the
/// leader, so a non-leader forwards the trigger over the same
/// HMAC-authenticated node-to-node channel.
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

/// Node-to-node only: what `trigger_pull` forwards to when this node is not
/// the leader.
async fn internal_trigger(
    _auth: InternalAuth,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    run_pull(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `clouduler git-backup pull`: manually run the same pull the webhook would
/// have triggered, without waiting for (or even configuring) one. Internal
/// auth rather than the webhook token.
async fn pull_now(
    _auth: InternalAuth,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    trigger_pull(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Trigger a reconcile on the leader. If called on a non-leader, forwards
/// to the leader over the internal authenticated channel.
async fn trigger_reconcile(state: &AppState) -> Result<(), ApiError> {
    if !state.cluster.is_leader() {
        let leader = leader_node_name(state)
            .await
            .ok_or_else(|| ApiError::Unavailable("no cluster leader known right now".into()))?;
        return internal_peer::post_empty(
            state,
            &leader,
            "/api/git-backup/webhook/internal-reconcile",
        )
        .await;
    }
    run_reconcile(state).await
}

/// Node-to-node only: what `trigger_reconcile` forwards to when this node is not the leader.
async fn internal_reconcile(
    _auth: InternalAuth,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    run_reconcile(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `clouduler git-backup reconcile`: manually force the cluster to reconcile
/// with the git repo: builds cluster.yaml, stacks, configs, and secrets from Raft
/// data, commits and pushes to git, and updates the last known good commit.
async fn reconcile_now(
    _auth: InternalAuth,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    trigger_reconcile(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn run_reconcile(state: &AppState) -> Result<(), ApiError> {
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
    let files = cl_backup::tree::build(
        &cluster_state,
        &key,
        &recipient,
        &mut cache_state.secret_cache,
    );

    let dir = state.data_dir.join("git-backup");
    let commit_oid = tokio::task::spawn_blocking(move || {
        let _repo_guard = cl_backup::git::lock_repo();
        let backup = cl_backup::git::GitBackup::open_or_init(
            &dir,
            &backup_cfg.repo_url,
            &backup_cfg.branch,
        )?;
        backup.reconcile(
            &files,
            "reconcile: mirror cluster state",
            &private_key_pem,
            None,
        )
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;

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

/// What the blocking git2 half of a pull decided, handed back to async code
/// to either apply (raft submissions) or just report.
enum PullDecision {
    /// Fetched tip is the same as the remote's tip - nothing to do.
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

/// Public so `cl-daemon`'s `watch_git_backup` can pull through this exact
/// path right before an export would otherwise rebuild the tracked tree
/// straight from raft state - see that function's doc comment for the race
/// this closes.
pub async fn run_pull(state: &AppState) -> Result<(), ApiError> {
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
    let private_key_pem =
        cl_backup::deploy_key::load_private(&cluster_state, &key).ok_or_else(|| {
            ApiError::Conflict(
                "no git deploy key yet - it is generated after the first export".into(),
            )
        })?;
    let existing_secrets: BTreeSet<String> = cluster_state.secrets.keys().cloned().collect();
    let existing_stacks = cluster_state.stacks.len();
    let last_good_oid =
        last_known_good_commit(&cluster_state).and_then(|s| git2::Oid::from_str(&s).ok());

    let dir = state.data_dir.join("git-backup");
    let decision = tokio::task::spawn_blocking(move || {
        pull_and_decide(
            &dir,
            &backup_cfg,
            &private_key_pem,
            &existing_secrets,
            existing_stacks,
            last_good_oid,
        )
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;

    match decision {
        PullDecision::NoChange => Ok(()),
        PullDecision::Rejected {
            reason,
            preserved_branch,
        } => {
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
            .await
        }
        PullDecision::Accepted {
            candidate,
            cluster_config,
            parsed,
        } => {
            apply_pulled_tree(state, &cluster_config, &parsed).await?;
            // No need to advance any local ref — the export path is
            // stateless (always fetches fresh). The next export's
            // fetch will see the same remote tip the pull just accepted.

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

/// The blocking git2 half of a pull: fetch the remote, compare the secrets
/// subtree against the cluster's last known good commit (or reject if secrets
/// changed, since git cannot authoritatively supply new secret content), parse
/// the candidate tree directly from the odb, and validate.
fn pull_and_decide(
    dir: &Path,
    cfg: &cl_config::GitBackupConfig,
    private_key_pem: &str,
    existing_secrets: &BTreeSet<String>,
    existing_stacks: usize,
    last_good_oid: Option<git2::Oid>,
) -> cl_backup::Result<PullDecision> {
    // One on-disk repo, shared with the export loop and manual reconcile.
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

    // Check whether the secrets/ subtree changed — if so, reject.
    if backup.secrets_subtree_changed(last_good_oid, candidate)? {
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

    // Parse the candidate tree directly from the odb — no working
    // directory checkout needed.
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

/// Parse + validate only, no cluster state beyond `existing_secrets` (a
/// stack's `file_secrets` requirement can only ever be satisfied by a secret
/// this cluster already holds - git can never supply new secret content, see
/// the git-backup design notes). A raft-submission failure after this passes
/// is not treated as malformed; see the git-backup design notes.
fn validate_parsed(
    parsed: &ParsedTree,
    existing_secrets: &BTreeSet<String>,
    existing_stacks: usize,
) -> Result<ClusterConfig, String> {
    let Some(yaml) = &parsed.cluster_yaml else {
        return Err("no cluster.yaml in the pulled tree".to_string());
    };
    let cluster_config = ClusterConfig::parse(yaml).map_err(|e| format!("cluster.yaml: {e}"))?;

    // `apply_pulled_tree` removes every cluster stack the tree does not
    // name, so a tree with no stacks at all (an accidental `git rm -r
    // stacks/`, a bad merge, a force-push of an unrelated repo that happens
    // to hold a cluster.yaml) would tear down the whole cluster's workloads.
    // Deliberately emptying a cluster goes through the api, not git.
    if parsed.stacks.is_empty() && existing_stacks > 0 {
        return Err(format!(
            "the pulled tree has no stacks/ at all, which would remove all {existing_stacks} stack(s) \
             this cluster runs - remove stacks through the api, not by emptying the repo"
        ));
    }

    for (name, spec) in &parsed.stacks {
        let stack = StackFile::parse(spec).map_err(|e| format!("stacks/{name}.yaml: {e}"))?;
        stack
            .validate(&cluster_config)
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
    Ok(cluster_config)
}

/// Reconciles the cluster to match a validated pulled tree: the cluster
/// config, every stack (deployed or removed), and the plain configs they
/// reference (deployed, orphaned to no owner, or removed).
async fn apply_pulled_tree(
    state: &AppState,
    cluster_config: &ClusterConfig,
    parsed: &ParsedTree,
) -> Result<(), ApiError> {
    let current = state.cluster.state().await;
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
        // `DeployStack` always bumps the stack's revision, unconditionally -
        // that revision feeds straight into every one of its instances'
        // `clouduler.revision` label (`cl_sched::spec::build`), so an
        // unconditional call here would recreate every container of every
        // stack in the repo on every pull, not just the one(s) whose spec
        // actually changed. Skip a stack whose pulled spec is byte-for-byte
        // what the cluster already has.
        if current.stacks.get(name).is_some_and(|s| &s.spec == spec) {
            continue;
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
        }
    }

    /// The bug a manual git push used to trigger: every stack named in a
    /// pulled tree got redeployed (bumping its revision, and with it every
    /// instance's `clouduler.revision` label - see `cl_sched::spec::build`),
    /// which recreates the container, even when that stack's spec was
    /// byte-for-byte unchanged. Only a stack whose pulled spec actually
    /// differs from what the cluster already has may be redeployed.
    #[tokio::test]
    async fn a_pull_does_not_redeploy_an_unchanged_stack() {
        let h = harness().await;
        h.state
            .cluster
            .submit(ClusterRequest::DeployStack {
                name: "site".to_string(),
                spec: SPEC.to_string(),
            })
            .await
            .expect("deploy");
        let before = h.state.cluster.state().await;
        assert_eq!(before.stacks["site"].revision, 1);

        let cluster_config = h.state.cluster_config.read().await.clone();
        apply_pulled_tree(&h.state, &cluster_config, &tree(&[("site", SPEC)]))
            .await
            .expect("apply unchanged tree");

        let after = h.state.cluster.state().await;
        assert_eq!(
            after.stacks["site"].revision, 1,
            "an unchanged spec must not bump the stack's revision"
        );
    }

    const CLUSTER_YAML: &str = include_str!("../../../../examples/cluster.yaml");

    /// A pulled tree with no stacks would make `apply_pulled_tree` remove
    /// every stack the cluster runs.
    #[test]
    fn a_tree_with_no_stacks_is_rejected_when_the_cluster_has_some() {
        let mut parsed = tree(&[]);
        parsed.cluster_yaml = Some(CLUSTER_YAML.to_string());

        let reason = validate_parsed(&parsed, &BTreeSet::new(), 3)
            .expect_err("an empty-stacks tree must be rejected");
        assert!(reason.contains("no stacks"), "unexpected reason: {reason}");

        // A cluster with nothing deployed yet may legitimately pull an empty tree.
        assert!(validate_parsed(&parsed, &BTreeSet::new(), 0).is_ok());
    }

    /// The other half: a stack whose spec really did change must still be
    /// redeployed.
    #[tokio::test]
    async fn a_pull_redeploys_a_changed_stack() {
        let h = harness().await;
        h.state
            .cluster
            .submit(ClusterRequest::DeployStack {
                name: "site".to_string(),
                spec: SPEC.to_string(),
            })
            .await
            .expect("deploy");

        let cluster_config = h.state.cluster_config.read().await.clone();
        apply_pulled_tree(&h.state, &cluster_config, &tree(&[("site", SPEC_V2)]))
            .await
            .expect("apply changed tree");

        let after = h.state.cluster.state().await;
        assert_eq!(after.stacks["site"].revision, 2);
        assert_eq!(after.stacks["site"].spec, SPEC_V2);
    }
}
