//! `/api/status`, `/api/state`, `/api/events`.

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::auth::Identity;
use crate::server::AppState;
use crate::stream;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/state", get(state))
        .route("/api/events", get(events))
}

#[derive(Serialize)]
struct StatusBody {
    /// The node answering this request, which is not necessarily the leader.
    node: String,
    is_leader: bool,
    has_quorum: bool,
    /// Leader's node name, not its raft id. The id is an internal hash of the
    /// name and means nothing to anyone reading the ui or scripting the api.
    leader: Option<String>,
    voters: usize,
    reachable: usize,
    managers_total: usize,
    managers_reachable: usize,
    /// Build version of the daemon answering.
    version: String,
    /// Replicated state version, which moves on every applied command.
    state_version: u64,
}

async fn status(_identity: Identity, State(state): State<AppState>) -> Json<StatusBody> {
    let quorum = state.cluster.quorum().status();
    let cluster_state = state.cluster.state().await;

    let managers: Vec<&str> = cluster_state
        .nodes
        .values()
        .filter(|n| n.role == cl_raft::Role::Manager)
        .map(|n| n.name.as_str())
        .collect();
    let managers_reachable = managers
        .iter()
        .filter(|name| !cluster_state.unreachable.contains(**name))
        .count();

    Json(StatusBody {
        node: state.node_name.clone(),
        is_leader: state.cluster.is_leader(),
        has_quorum: quorum.has_quorum,
        leader: quorum.leader.and_then(|id| node_name_for(&cluster_state, id)),
        voters: quorum.voters,
        reachable: quorum.reachable.len(),
        managers_total: managers.len(),
        managers_reachable,
        version: env!("CARGO_PKG_VERSION").to_string(),
        state_version: cluster_state.version,
    })
}

/// Raft ids are a hash of the node's config-declared uuid, so the only way
/// back is to hash every known node's id and compare. The roster is a
/// handful of entries, so that is fine.
fn node_name_for(state: &cl_raft::ClusterState, id: u64) -> Option<String> {
    state
        .nodes
        .values()
        .find(|node| cl_raft::node_id(node.id) == id)
        .map(|node| node.name.clone())
}

async fn state(_identity: Identity, State(state): State<AppState>) -> impl IntoResponse {
    Json(state.cluster.state().await.redacted())
}

async fn events(_identity: Identity, State(state): State<AppState>) -> impl IntoResponse {
    stream::cluster_state_events(state.cluster.clone())
}
