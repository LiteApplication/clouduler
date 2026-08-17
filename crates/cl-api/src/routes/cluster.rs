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
    node: String,
    is_leader: bool,
    has_quorum: bool,
    leader: Option<String>,
    voters: usize,
    reachable: usize,
    version: u64,
}

async fn status(_identity: Identity, State(state): State<AppState>) -> Json<StatusBody> {
    let quorum = state.cluster.quorum().status();
    let cluster_state = state.cluster.state().await;
    Json(StatusBody {
        node: state.node_name.clone(),
        is_leader: state.cluster.is_leader(),
        has_quorum: quorum.has_quorum,
        leader: quorum.leader.map(|id| id.to_string()),
        voters: quorum.voters,
        reachable: quorum.reachable.len(),
        version: cluster_state.version,
    })
}

async fn state(_identity: Identity, State(state): State<AppState>) -> impl IntoResponse {
    Json(state.cluster.state().await.redacted())
}

async fn events(_identity: Identity, State(state): State<AppState>) -> impl IntoResponse {
    stream::cluster_state_events(state.cluster.clone())
}
