//! `/api/nodes/connectivity`: who can reach whom.
//!
//! Every node probes its peers itself and reports what *it* sees, so a break
//! that only goes one way (A reaches B, B does not reach A) shows up as two
//! different reports instead of being averaged away by the leader's single
//! replicated `unreachable` list. The aggregate asks every node for its own
//! report over the signed node-to-node channel (see `internal_peer`); a node
//! that cannot be asked is itself reported as having no report.
//!
//! A "link" here is a plain TCP connect from this node to the peer's raft rpc
//! port and to its web/api port, both on the internal network. That is the
//! reachability raft heartbeats and node-to-node api forwarding depend on.
//! The vxlan overlay and drive-sync links are not probed separately.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use cl_config::ClusterConfig;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tracing::warn;

use crate::auth::internal::InternalAuth;
use crate::auth::Identity;
use crate::error::ApiError;
use crate::internal_peer;
use crate::server::AppState;

/// Longer than a healthy lan connect, short enough that a whole probe round
/// stays well inside the 5s `internal_peer` fetch timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

const INTERNAL_LINKS_PATH: &str = "/api/nodes/internal/links";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/nodes/connectivity", get(connectivity))
        .route(INTERNAL_LINKS_PATH, get(internal_links))
}

/// What one node sees of one peer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PeerLink {
    pub peer: String,
    /// A tcp connect to the peer's raft rpc address succeeded.
    pub raft: bool,
    /// A tcp connect to the peer's web/api port succeeded.
    pub api: bool,
    /// Round trip of the raft connect, when it succeeded.
    pub raft_ms: Option<u64>,
}

/// One node's own view of every peer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct NodeLinks {
    pub node: String,
    pub links: Vec<PeerLink>,
}

/// One node in the aggregate: `reported` is false when the node could not be
/// asked, in which case `links` is empty and says nothing about its peers.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct NodeReport {
    pub node: String,
    pub reported: bool,
    pub links: Vec<PeerLink>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Connectivity {
    pub reports: Vec<NodeReport>,
}

async fn probe(addr: SocketAddr) -> Option<u64> {
    let start = Instant::now();
    match tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(addr)).await {
        Ok(Ok(_)) => Some(start.elapsed().as_millis() as u64),
        _ => None,
    }
}

/// Probe every other configured node from here.
pub(crate) async fn local_links(self_name: &str, cfg: &ClusterConfig) -> NodeLinks {
    let probes = cfg
        .nodes
        .iter()
        .filter(|(name, _)| name.as_str() != self_name)
        .map(|(name, node)| {
            let raft_addr = cfg.rpc_addr(node);
            let api_addr = cfg
                .internal_ip(node)
                .map(|ip| SocketAddr::new(ip, cfg.web.port));
            let peer = name.clone();
            async move {
                let (raft_ms, api_ms) = futures::future::join(
                    async {
                        match raft_addr {
                            Some(a) => probe(a).await,
                            None => None,
                        }
                    },
                    async {
                        match api_addr {
                            Some(a) => probe(a).await,
                            None => None,
                        }
                    },
                )
                .await;
                PeerLink {
                    peer,
                    raft: raft_ms.is_some(),
                    api: api_ms.is_some(),
                    raft_ms,
                }
            }
        });
    NodeLinks {
        node: self_name.to_string(),
        links: futures::future::join_all(probes).await,
    }
}

/// The body of `connectivity`, callable in-process.
pub(crate) async fn collect(state: &AppState) -> Connectivity {
    let cfg = state.cluster_config.read().await.clone();
    let unreachable = state.cluster.state().await.unreachable;

    let asks = cfg.nodes.keys().map(|name| {
        let cfg = &cfg;
        let unreachable = &unreachable;
        async move {
            let links = if *name == state.node_name {
                Some(local_links(name, cfg).await.links)
            } else if unreachable.contains(name) {
                // Already known down: do not pay the fetch timeout on every poll.
                None
            } else {
                match internal_peer::fetch_json::<NodeLinks>(state, name, INTERNAL_LINKS_PATH).await
                {
                    Ok(report) => Some(report.links),
                    Err(e) => {
                        warn!(node = %name, error = %e, "could not fetch a connectivity report");
                        None
                    }
                }
            };
            NodeReport {
                node: name.clone(),
                reported: links.is_some(),
                links: links.unwrap_or_default(),
            }
        }
    });
    Connectivity {
        reports: futures::future::join_all(asks).await,
    }
}

async fn connectivity(
    _identity: Identity,
    State(state): State<AppState>,
) -> Result<Json<Connectivity>, ApiError> {
    Ok(Json(collect(&state).await))
}

/// Node-to-node: always this node's own view.
async fn internal_links(_auth: InternalAuth, State(state): State<AppState>) -> Json<NodeLinks> {
    let cfg = state.cluster_config.read().await.clone();
    Json(local_links(&state.node_name, &cfg).await)
}
