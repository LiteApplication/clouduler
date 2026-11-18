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
pub(crate) struct StatusBody {
    pub(crate) node: String,
    pub(crate) is_leader: bool,
    pub(crate) has_quorum: bool,
    /// A node name, not its raft id, which is an internal hash meaningless to ui or script readers.
    pub(crate) leader: Option<String>,
    pub(crate) voters: usize,
    pub(crate) reachable: usize,
    pub(crate) managers_total: usize,
    pub(crate) managers_reachable: usize,
    pub(crate) version: String,
    pub(crate) state_version: u64,
}

async fn status(_identity: Identity, State(state): State<AppState>) -> Json<StatusBody> {
    Json(cluster_status(&state).await)
}

pub(crate) async fn cluster_status(state: &AppState) -> StatusBody {
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

    StatusBody {
        node: state.node_name.clone(),
        is_leader: state.cluster.is_leader(),
        has_quorum: quorum.has_quorum,
        leader: quorum
            .leader
            .and_then(|id| node_name_for(&cluster_state, id)),
        voters: quorum.voters,
        reachable: quorum.reachable.len(),
        managers_total: managers.len(),
        managers_reachable,
        version: env!("CARGO_PKG_VERSION").to_string(),
        state_version: cluster_state.version,
    }
}

/// Raft ids hash the node's config-declared uuid, so the way back is hashing every known node's id; the roster is tiny.
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
    stream::cluster_state_events(state.cluster)
}
