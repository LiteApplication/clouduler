//! `/api/instances`.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_config::{Service, StackFile};
use cl_docker::{Docker, ExecSpec, LogOptions, Tail};
use cl_raft::ids::InstanceId;
use cl_raft::state::DesiredState;
use cl_raft::Request as ClusterRequest;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tracing::warn;

use crate::auth::internal::{InternalAuth, InternalBodyAuth};
use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::internal_peer;
use crate::server::AppState;
use crate::stream;

mod actions;
mod exec;
mod files;
mod lifecycle;
mod logs;
mod summary;
mod update;

pub use actions::*;
pub(crate) use exec::*;
pub(crate) use files::*;
pub(crate) use lifecycle::*;
pub(crate) use logs::*;
pub(crate) use summary::*;
pub(crate) use update::*;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/instances", get(list))
        .route("/api/instances/{id}", get(detail).patch(patch_instance))
        .route("/api/instances/{id}/restart", post(restart))
        .route("/api/instances/{id}/actions", get(list_actions))
        .route("/api/instances/{id}/actions/{name}", post(run_action))
        .route("/api/instances/{id}/move", post(move_instance))
        .route("/api/instances/{id}/migration/retry", post(retry_migration))
        .route(
            "/api/instances/{id}/migration/dismiss",
            post(dismiss_migration),
        )
        .route("/api/rebalance", post(rebalance))
        .route("/api/rebalance/last", get(last_rebalance))
        .route("/api/instances/{id}/update", post(update))
        .route("/api/instances/{id}/logs", get(logs))
        .route("/api/instances/{id}/stats", get(stats))
        .route("/api/instances/events", get(instance_events))
        // Node-to-node only, HMAC authenticated: forwarded to when the instance lives on another node.
        .route(
            "/api/instances/internal/{id}/summary",
            get(internal_summary),
        )
        .route("/api/instances/internal/{id}/logs", get(internal_logs))
        .route("/api/instances/internal/{id}/stats", get(internal_stats))
        .route(
            crate::live_usage::NODE_LIVE_USAGE_PATH,
            get(internal_live_usage),
        )
        .route(
            "/api/instances/internal/cluster-live-usage",
            get(internal_cluster_live_usage),
        )
        .route("/api/instances/internal/{id}/update", post(internal_update))
        .route("/api/instances/internal/{id}/pull", post(internal_pull))
        .route(
            "/api/instances/internal/{id}/replace",
            post(internal_replace),
        )
        .route(
            "/api/instances/internal/{id}/remove-container",
            post(internal_remove_container),
        )
        // Node-to-node only: the cli resolves the instance's node itself, so there is no public forwarding counterpart.
        .route("/api/instances/internal/{id}/exec", get(internal_exec))
        // Node-to-node only: `clouduler capture` connects here directly.
        .route(
            "/api/instances/internal/{id}/capture",
            get(internal_capture),
        )
        .route(
            "/api/instances/internal/{id}/exec-run",
            post(internal_exec_run),
        )
        .route("/api/instances/internal/{id}/file", get(internal_file))
}

pub(super) fn parse_id(raw: &str) -> Result<InstanceId, ApiError> {
    raw.parse().map_err(ApiError::BadRequest)
}

/// Runs `local` when `instance` is assigned to this node, otherwise `remote` with the owning node's name.
pub(super) async fn forward_or_local<T, L, R, RF>(
    state: &AppState,
    instance: &InstanceId,
    local: L,
    remote: R,
) -> Result<T, ApiError>
where
    L: std::future::Future<Output = Result<T, ApiError>>,
    R: FnOnce(String) -> RF,
    RF: std::future::Future<Output = Result<T, ApiError>>,
{
    let node = owning_node(state, instance).await?;
    if node == state.node_name {
        local.await
    } else {
        remote(node).await
    }
}

pub(crate) async fn owning_node(
    state: &AppState,
    instance: &InstanceId,
) -> Result<String, ApiError> {
    state
        .cluster
        .state()
        .await
        .assignments
        .get(instance)
        .map(|a| a.node.clone())
        .ok_or_else(|| ApiError::NotFound(format!("no instance {instance}")))
}

/// Ceiling on one buffered command; the forwarding client's write timeout is also 300s.
pub(super) const EXEC_RUN_MAX_SECS: u64 = 300;
pub(super) const EXEC_RUN_DEFAULT_SECS: u64 = 30;
/// Ceiling per stream, stdin and file read: the answer travels as json into a model's context.
pub(super) const MAX_BUFFERED_BYTES: usize = 1024 * 1024;
pub(super) const EXEC_RUN_DEFAULT_OUTPUT: usize = 64 * 1024;
pub(super) const FILE_READ_DEFAULT: usize = 256 * 1024;
pub(super) const FILE_LIST_LIMIT: usize = 500;
pub(super) const BINARY_SNIFF_BYTES: usize = 8 * 1024;
pub(super) const LOG_TAIL_LIMIT: u32 = 10_000;

pub(super) fn docker_error(e: cl_docker::DockerError) -> ApiError {
    match e {
        cl_docker::DockerError::NotFound(what) => ApiError::NotFound(what),
        other => ApiError::internal(other),
    }
}
