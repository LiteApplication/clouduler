//! `/api/nodes`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
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
use crate::node_logs;
use crate::node_metrics::{self, DriveSpace, Sample};
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/nodes", get(list).post(add))
        .route(
            "/api/nodes/{name}",
            get(detail).patch(patch_node).delete(remove),
        )
        .route("/api/nodes/metrics", get(metrics))
        .route("/api/nodes/join-command", post(join_command))
        .route("/api/nodes/{name}/metrics/history", get(metrics_history))
        .route("/api/nodes/{name}/logs", get(logs))
        .route("/api/nodes/{name}/interfaces", get(interfaces))
        // Node-to-node only, HMAC authenticated: what `detail`/`interfaces` forward to for another node.
        .route("/api/nodes/internal/{name}/usage", get(internal_usage))
        .route(
            "/api/nodes/internal/{name}/metrics/history",
            get(internal_metrics_history),
        )
        .route("/api/nodes/internal/{name}/logs", get(internal_logs))
        .route(
            "/api/nodes/internal/{name}/log-tail",
            get(internal_log_tail),
        )
        .route(
            "/api/nodes/internal/{name}/interfaces",
            get(internal_interfaces),
        )
}

#[derive(Serialize)]
pub(crate) struct NodeSummary {
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) availability: String,
    pub(crate) reachable: bool,
    pub(crate) container_count: usize,
}

/// Every node in the config, not just those admitted to raft: a configured node that is down
/// must show as unreachable, to tell "still joining" from "safe to add".
async fn list(_identity: Identity, State(state): State<AppState>) -> Json<Vec<NodeSummary>> {
    Json(list_nodes(&state).await)
}

pub(crate) async fn list_nodes(state: &AppState) -> Vec<NodeSummary> {
    let cluster_state = state.cluster.snapshot().await;
    let config = state.cluster_config.read().await;

    let nodes = config
        .nodes
        .iter()
        .map(|(name, configured)| match cluster_state.nodes.get(name) {
            Some(live) => NodeSummary {
                name: live.name.clone(),
                role: format!("{:?}", live.role).to_lowercase(),
                availability: format!("{:?}", live.availability).to_lowercase(),
                reachable: !cluster_state.unreachable.contains(name),
                container_count: cluster_state.assignments_on(name).count(),
            },
            None => NodeSummary {
                name: name.clone(),
                role: format!("{:?}", configured.role).to_lowercase(),
                availability: "pending".to_string(),
                reachable: false,
                container_count: 0,
            },
        })
        .collect();
    nodes
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct ResourceUsage {
    pub(crate) cpu_count: usize,
    load_1m: f64,
    pub(crate) memory_total_bytes: u64,
    pub(crate) memory_used_bytes: u64,
    pub(crate) uptime_seconds: u64,
    #[serde(default)]
    pub(crate) cpu_percent: f32,
    #[serde(default)]
    pub(crate) version: String,
    #[serde(default)]
    pub(crate) drives: Vec<DriveSpace>,
    /// False for the zeroed default returned when neither a local read nor forwarding succeeded.
    pub(crate) local: bool,
}

#[derive(Deserialize)]
struct JoinCommandRequest {
    name: String,
    #[serde(default)]
    role: Option<cl_raft::state::Role>,
    #[serde(default)]
    address: Option<std::net::IpAddr>,
}

#[derive(Serialize)]
struct JoinCommand {
    command: String,
    key_id: String,
    address: String,
    fingerprint: String,
}

/// The key is shown only here.
async fn join_command(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<JoinCommandRequest>,
) -> Result<Json<JoinCommand>, ApiError> {
    let name = body.name.trim().to_string();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return Err(ApiError::BadRequest(
            "name must be a single word with no spaces".into(),
        ));
    }
    let (address, role) = {
        let config = state.cluster_config.read().await;
        (
            format!("{}:{}", config.web_fqdn(), config.web.port),
            body.role.unwrap_or(cl_raft::state::Role::Worker),
        )
    };
    let role = match role {
        cl_raft::state::Role::Manager => "manager",
        cl_raft::state::Role::Worker => "worker",
    };

    let cluster_state = state.cluster.snapshot().await;
    let mut store = crate::auth::keys::ApiKeyStore::load(&cluster_state);
    let new_key =
        crate::auth::keys::generate(Some(format!("join {name}")), crate::auth::oauth::now_secs());
    let key_id = new_key.record.id.clone();
    store.keys.push(new_key.record);
    let response = state
        .cluster
        .submit(ClusterRequest::PutConfig {
            name: crate::auth::keys::API_KEYS_CONFIG.to_string(),
            content: store.encode(),
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Internal(msg));
    }

    let ip = body
        .address
        .map(|ip| format!(" --ip {ip}"))
        .unwrap_or_default();
    let command = format!(
        "clouduler init --yes --join {address} --token {} --tls-fingerprint {} --node {name} --role {role}{ip}",
        new_key.secret, state.tls_fingerprint
    );
    Ok(Json(JoinCommand {
        command,
        key_id,
        address,
        fingerprint: state.tls_fingerprint.clone(),
    }))
}

#[derive(Serialize)]
struct NodeMetrics {
    name: String,
    container_count: usize,
    usage: ResourceUsage,
}

const METRICS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

async fn metrics(_identity: Identity, State(state): State<AppState>) -> Json<Vec<NodeMetrics>> {
    let cluster_state = state.cluster.snapshot().await;
    let reads = cluster_state
        .nodes
        .values()
        .filter(|n| n.name == state.node_name || !cluster_state.unreachable.contains(&n.name))
        .map(|node| {
            let state = &state;
            let cluster_state = &cluster_state;
            async move {
                let usage = if node.name == state.node_name {
                    local_resource_usage(&node.drives)
                } else {
                    internal_peer::fetch_json_within::<ResourceUsage>(
                        state,
                        &node.name,
                        &internal_usage_path(&node.name),
                        METRICS_TIMEOUT,
                    )
                    .await
                    .ok()?
                };
                Some(NodeMetrics {
                    name: node.name.clone(),
                    container_count: cluster_state.assignments_on(&node.name).count(),
                    usage,
                })
            }
        });
    Json(
        futures::future::join_all(reads)
            .await
            .into_iter()
            .flatten()
            .collect(),
    )
}

async fn metrics_history(
    _identity: Identity,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Vec<Sample>>, ApiError> {
    if name == state.node_name {
        return Ok(Json(node_metrics::history()));
    }
    let cluster_state = state.cluster.snapshot().await;
    if !cluster_state.nodes.contains_key(&name) {
        return Err(ApiError::NotFound(format!("no node {name}")));
    }
    if cluster_state.unreachable.contains(&name) {
        return Ok(Json(Vec::new()));
    }
    let samples = internal_peer::fetch_json_within(
        &state,
        &name,
        &format!("/api/nodes/internal/{name}/metrics/history"),
        METRICS_TIMEOUT,
    )
    .await?;
    Ok(Json(samples))
}

async fn internal_metrics_history(
    _auth: InternalAuth,
    State(_state): State<AppState>,
) -> Json<Vec<Sample>> {
    Json(node_metrics::history())
}

#[derive(Serialize)]
pub(crate) struct NodeDetail {
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) availability: String,
    pub(crate) reachable: bool,
    pub(crate) drives: Vec<String>,
    pub(crate) container_count: usize,
    pub(crate) usage: ResourceUsage,
}

async fn detail(
    _identity: Identity,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<NodeDetail>, ApiError> {
    Ok(Json(node_detail(&state, name).await?))
}

pub(crate) async fn node_detail(state: &AppState, name: String) -> Result<NodeDetail, ApiError> {
    let cluster_state = state.cluster.snapshot().await;
    let node = cluster_state.nodes.get(&name);

    let Some(node) = node else {
        // Configured but not yet admitted to raft (see `list`).
        let config = state.cluster_config.read().await;
        let configured = config
            .nodes
            .get(&name)
            .ok_or_else(|| ApiError::NotFound(format!("no node {name}")))?;
        return Ok(NodeDetail {
            name,
            role: format!("{:?}", configured.role).to_lowercase(),
            availability: "pending".to_string(),
            reachable: false,
            drives: Vec::new(),
            container_count: 0,
            usage: ResourceUsage::default(),
        });
    };

    let usage = if name == state.node_name {
        local_resource_usage(&node.drives)
    } else if cluster_state.unreachable.contains(&name) {
        // Already known down: skip the round trip instead of paying its timeout on every poll.
        ResourceUsage::default()
    } else {
        internal_peer::fetch_json(state, &name, &internal_usage_path(&name))
            .await
            .unwrap_or_else(|e| {
                warn!(node = %name, error = %e, "could not fetch resource usage from its node");
                ResourceUsage::default()
            })
    };

    Ok(NodeDetail {
        name: node.name.clone(),
        role: format!("{:?}", node.role).to_lowercase(),
        availability: format!("{:?}", node.availability).to_lowercase(),
        reachable: !cluster_state.unreachable.contains(&name),
        drives: node.drives.keys().cloned().collect(),
        container_count: cluster_state.assignments_on(&name).count(),
        usage,
    })
}

fn internal_usage_path(name: &str) -> String {
    format!("/api/nodes/internal/{name}/usage")
}

/// Always this node's own reading; a node only forwards to the node it is asking about.
async fn internal_usage(_auth: InternalAuth, State(state): State<AppState>) -> Json<ResourceUsage> {
    let cluster_state = state.cluster.snapshot().await;
    let drives = cluster_state
        .nodes
        .get(&state.node_name)
        .map(|n| n.drives.clone())
        .unwrap_or_default();
    Json(local_resource_usage(&drives))
}

pub(crate) fn local_resource_usage(
    drives: &std::collections::BTreeMap<String, std::path::PathBuf>,
) -> ResourceUsage {
    let mut sys = System::new();
    sys.refresh_all();
    let load = System::load_average();
    ResourceUsage {
        cpu_count: sys.cpus().len(),
        load_1m: load.one,
        memory_total_bytes: sys.total_memory(),
        memory_used_bytes: sys.used_memory(),
        uptime_seconds: System::uptime(),
        cpu_percent: node_metrics::latest_cpu_percent(),
        version: cl_config::version().to_string(),
        drives: node_metrics::drive_space(drives),
        local: true,
    }
}

/// For `cl-daemon`'s `report_self` (`NodeState::memory_total_bytes`).
pub fn total_memory_bytes() -> u64 {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.total_memory()
}

/// Host interfaces an operator might use as an `egress`/`wireguard` node address (not managed by
/// clouduler or docker). Best-effort autofill; the field stays free text.
fn is_managed_interface(name: &str) -> bool {
    name == "lo"
        || name.starts_with("docker")
        || name.starts_with("veth")
        || name.starts_with("br-")
        || name.starts_with("clbr-")
        || name.starts_with("clvx-")
        || name.starts_with("clex-")
}

fn local_interfaces() -> Vec<String> {
    let networks = sysinfo::Networks::new_with_refreshed_list();
    let mut names: Vec<String> = networks
        .keys()
        .filter(|name| !is_managed_interface(name))
        .cloned()
        .collect();
    names.sort();
    names
}

async fn interfaces(
    _identity: Identity,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Vec<String>>, ApiError> {
    if name == state.node_name {
        return Ok(Json(local_interfaces()));
    }
    let cluster_state = state.cluster.snapshot().await;
    if cluster_state.unreachable.contains(&name) {
        return Ok(Json(Vec::new()));
    }
    let names = internal_peer::fetch_json(&state, &name, &internal_interfaces_path(&name))
        .await
        .unwrap_or_else(|e| {
            warn!(node = %name, error = %e, "could not fetch interfaces from its node");
            Vec::new()
        });
    Ok(Json(names))
}

fn internal_interfaces_path(name: &str) -> String {
    format!("/api/nodes/internal/{name}/interfaces")
}

async fn internal_interfaces(
    _auth: InternalAuth,
    State(_state): State<AppState>,
) -> Json<Vec<String>> {
    Json(local_interfaces())
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
            id: uuid::Uuid::new_v4(),
            role: body.role,
            rpc_addr: body.rpc_addr,
            drives: body.drives,
            labels: body.labels,
            last_resort: false,
            availability: Availability::Active,
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

/// Removes the node from the roster and raft membership (reconciled asynchronously by the leader,
/// `reap_removed_nodes`), then from the cluster config so a manager restart does not re-add it.
///
/// The config edit is validated *before* touching the roster: a node can be named in
/// `drives.*.served_by`, so dropping it from `config.nodes` can fail validation after `RemoveNode`
/// has committed, leaving the node tombstoned (never able to rejoin) with its config entry
/// permanently unremovable.
async fn remove(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut config = state.cluster_config.read().await.clone();
    let had_entry = config.nodes.remove(&name).is_some();
    if had_entry {
        config
            .validate()
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    }

    submit(&state, ClusterRequest::RemoveNode { node: name.clone() }).await?;

    if had_entry {
        super::config::apply_config(&state, &config).await?;
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct LogsQuery {
    tail: Option<usize>,
    follow: Option<bool>,
}

fn clamp_tail(tail: Option<usize>) -> usize {
    tail.unwrap_or(200).min(node_logs::CAPACITY)
}

fn log_event(line: &node_logs::LogLine) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .event("log")
        .json_data(line)
        .unwrap_or_else(|_| {
            axum::response::sse::Event::default()
                .event("log")
                .data("{}")
        })
}

/// Admin only, since daemon logs can name hosts and paths. Relayed from the node for another node.
async fn logs(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<LogsQuery>,
) -> Result<axum::response::Response, ApiError> {
    if name == state.node_name {
        return Ok(serve_logs(&query).into_response());
    }
    let cluster_state = state.cluster.snapshot().await;
    if !cluster_state.nodes.contains_key(&name) {
        return Err(ApiError::NotFound(format!("no node {name}")));
    }
    if cluster_state.unreachable.contains(&name) {
        return Err(ApiError::Unavailable(format!("node {name} is unreachable")));
    }
    let path = format!(
        "/api/nodes/internal/{name}/logs?tail={}&follow={}",
        clamp_tail(query.tail),
        query.follow.unwrap_or(true)
    );
    internal_peer::proxy_stream(&state, &name, &path).await
}

fn serve_logs(
    query: &LogsQuery,
) -> axum::response::sse::Sse<
    impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use futures::StreamExt;
    let (backlog, rx) = node_logs::ring().follow(clamp_tail(query.tail));
    let replay = futures::stream::iter(backlog);
    let live: futures::stream::BoxStream<'static, node_logs::LogLine> =
        if query.follow.unwrap_or(true) {
            // A follower that lags too far skips ahead rather than ending the stream.
            tokio_stream::wrappers::BroadcastStream::new(rx)
                .filter_map(|r| futures::future::ready(r.ok()))
                .boxed()
        } else {
            futures::stream::empty().boxed()
        };
    let events = replay.chain(live).map(|l| Ok(log_event(&l)));
    axum::response::sse::Sse::new(events).keep_alive(axum::response::sse::KeepAlive::default())
}

async fn internal_logs(
    _auth: InternalAuth,
    State(_state): State<AppState>,
    Query(query): Query<LogsQuery>,
) -> impl IntoResponse {
    serve_logs(&query)
}

async fn internal_log_tail(
    _auth: InternalAuth,
    State(_state): State<AppState>,
    Query(query): Query<LogsQuery>,
) -> Json<Vec<node_logs::LogLine>> {
    Json(node_logs::ring().tail(clamp_tail(query.tail)))
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
