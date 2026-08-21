//! `/api/nodes`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use cl_raft::state::{Availability, NodeState};
use cl_raft::Request as ClusterRequest;
use serde::{Deserialize, Serialize};
use sysinfo::System;
use tracing::warn;

use crate::auth::internal::InternalAuth;
use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::internal_peer;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/nodes", get(list).post(add))
        .route("/api/nodes/{name}", get(detail).patch(patch_node).delete(remove))
        .route("/api/nodes/{name}/logs", get(logs))
        // Node-to-node only, HMAC authenticated: what `detail` forwards to
        // when asked about a node other than the one answering. See
        // `internal_peer`.
        .route("/api/nodes/internal/{name}/usage", get(internal_usage))
}

#[derive(Serialize)]
struct NodeSummary {
    name: String,
    role: String,
    availability: String,
    reachable: bool,
    container_count: usize,
}

async fn list(_identity: Identity, State(state): State<AppState>) -> Json<Vec<NodeSummary>> {
    let cluster_state = state.cluster.state().await;
    let nodes = cluster_state
        .nodes
        .values()
        .map(|n| NodeSummary {
            name: n.name.clone(),
            role: format!("{:?}", n.role).to_lowercase(),
            availability: format!("{:?}", n.availability).to_lowercase(),
            reachable: !cluster_state.unreachable.contains(&n.name),
            container_count: cluster_state.assignments_on(&n.name).count(),
        })
        .collect();
    Json(nodes)
}

#[derive(Serialize, Deserialize, Default)]
struct ResourceUsage {
    cpu_count: usize,
    load_1m: f64,
    memory_total_bytes: u64,
    memory_used_bytes: u64,
    uptime_seconds: u64,
    /// True once this reflects a real reading rather than the zeroed
    /// default returned when neither a local read nor forwarding to the
    /// owning node succeeded.
    local: bool,
}

#[derive(Serialize)]
struct NodeDetail {
    name: String,
    role: String,
    availability: String,
    reachable: bool,
    drives: Vec<String>,
    container_count: usize,
    usage: ResourceUsage,
}

async fn detail(
    _identity: Identity,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<NodeDetail>, ApiError> {
    let cluster_state = state.cluster.state().await;
    let node = cluster_state
        .nodes
        .get(&name)
        .ok_or_else(|| ApiError::NotFound(format!("no node {name}")))?;

    let usage = if name == state.node_name {
        local_resource_usage()
    } else if cluster_state.unreachable.contains(&name) {
        // Already known down: skip the round trip rather than pay its
        // timeout on every poll of a node everyone already knows is gone.
        ResourceUsage::default()
    } else {
        internal_peer::fetch_json(&state, &name, &internal_usage_path(&name))
            .await
            .unwrap_or_else(|e| {
                warn!(node = %name, error = %e, "could not fetch resource usage from its node");
                ResourceUsage::default()
            })
    };

    Ok(Json(NodeDetail {
        name: node.name.clone(),
        role: format!("{:?}", node.role).to_lowercase(),
        availability: format!("{:?}", node.availability).to_lowercase(),
        reachable: !cluster_state.unreachable.contains(&name),
        drives: node.drives.keys().cloned().collect(),
        container_count: cluster_state.assignments_on(&name).count(),
        usage,
    }))
}

fn internal_usage_path(name: &str) -> String {
    format!("/api/nodes/internal/{name}/usage")
}

/// Node-to-node: always this node's own reading, regardless of `name` — a
/// node only ever forwards to the node it is actually asking about.
async fn internal_usage(_auth: InternalAuth, State(_state): State<AppState>) -> Json<ResourceUsage> {
    Json(local_resource_usage())
}

fn local_resource_usage() -> ResourceUsage {
    let mut sys = System::new();
    sys.refresh_all();
    let load = System::load_average();
    ResourceUsage {
        cpu_count: sys.cpus().len(),
        load_1m: load.one,
        memory_total_bytes: sys.total_memory(),
        memory_used_bytes: sys.used_memory(),
        uptime_seconds: System::uptime(),
        local: true,
    }
}

#[derive(Deserialize)]
struct AddNodeRequest {
    name: String,
    role: cl_raft::state::Role,
    rpc_addr: std::net::SocketAddr,
    #[serde(default)]
    drives: std::collections::BTreeMap<String, std::path::PathBuf>,
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
}

async fn add(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<AddNodeRequest>,
) -> Result<StatusCode, ApiError> {
    submit(
        &state,
        ClusterRequest::UpsertNode(NodeState {
            name: body.name,
            role: body.role,
            rpc_addr: body.rpc_addr,
            drives: body.drives,
            labels: body.labels,
            availability: Availability::Active,
            egress_networks: Default::default(),
            networks: Default::default(),
            open_ports: Default::default(),
            arch: String::new(),
            binary_hash: String::new(),
            node_pubkey: None,
        }),
    )
    .await?;
    Ok(StatusCode::CREATED)
}

#[derive(Deserialize)]
struct PatchNodeRequest {
    availability: Availability,
}

async fn patch_node(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<PatchNodeRequest>,
) -> Result<StatusCode, ApiError> {
    submit(
        &state,
        ClusterRequest::SetAvailability {
            node: name,
            availability: body.availability,
        },
    )
    .await?;
    Ok(StatusCode::OK)
}

async fn remove(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    submit(&state, ClusterRequest::RemoveNode { node: name }).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Daemon log tail for one node.
///
/// Only implemented for the node currently answering the request: cl-raft
/// has no rpc call to fetch another node's log buffer, and adding one is out
/// of scope for this crate. See docs/api.md for the note on this limitation.
async fn logs(
    _identity: Identity,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    if name != state.node_name {
        return Err(ApiError::NotFound(format!(
            "node {name} logs are only available by connecting to that node directly"
        )));
    }
    // No in-process log ring buffer is wired up yet; an empty stream is a
    // truthful placeholder until one exists.
    let empty = futures::stream::empty::<Result<axum::response::sse::Event, std::convert::Infallible>>();
    Ok(axum::response::sse::Sse::new(empty))
}

async fn submit(state: &AppState, request: ClusterRequest) -> Result<(), ApiError> {
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
