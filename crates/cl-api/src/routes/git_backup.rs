//! `GET /api/git-backup/status`: read-only view of the git backup feature.
//!
//! Unlike `self_update`'s write-once `PUT /api/self-update/repo`, `backup:`'s
//! fields (repo url, age recipient, branch, interval) are meant to be freely
//! editable, so they go through the existing `PUT /api/config` draft-editing
//! flow the ui already uses for `FailoverConfig`/`SecretsConfig` - this route
//! exists only to report what the leader has actually done with them.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use cl_backup::deploy_key::{public_from_state, DEPLOY_KEY_PRIVATE_SECRET, DEPLOY_KEY_PUBLIC_CONFIG};
use cl_backup::status::{load as load_last_export, LastExport, LAST_EXPORT_CONFIG};
use serde::Serialize;

use crate::auth::Identity;
use crate::server::AppState;

/// Config/secret names this feature owns and never shows through the
/// generic config/secret listing routes, mirroring `self_update`'s.
pub fn is_reserved_config(name: &str) -> bool {
    name == DEPLOY_KEY_PUBLIC_CONFIG || name == LAST_EXPORT_CONFIG
}

pub fn is_reserved_secret(name: &str) -> bool {
    name == DEPLOY_KEY_PRIVATE_SECRET
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/git-backup/status", get(status))
}

#[derive(Serialize)]
struct GitBackupStatus {
    /// True once `backup:` is set in the cluster config.
    configured: bool,
    /// The public half of the deploy key, once the leader has generated
    /// one - hand this to the git host as a write-capable deploy key.
    deploy_public_key: Option<String>,
    last_export: Option<LastExport>,
}

async fn status(_identity: Identity, State(state): State<AppState>) -> Json<GitBackupStatus> {
    let configured = state.cluster_config.read().await.backup.is_some();
    let cluster_state = state.cluster.state().await;
    Json(GitBackupStatus {
        configured,
        deploy_public_key: public_from_state(&cluster_state),
        last_export: load_last_export(&cluster_state),
    })
}
