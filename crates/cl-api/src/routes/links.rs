//! `/api/nodes/connectivity`: who can reach whom.
//!
//! Every node probes its peers itself and reports what *it* sees, so a break
//! that only goes one way (A reaches B, B does not reach A) shows up as two
//! different reports instead of being averaged away by the leader's single
//! replicated `unreachable` list. The aggregate asks every node for its own
//! report over the signed node-to-node channel (see `internal_peer`); a node
//! that cannot be asked is itself reported as having no report.
//!
//! The raft half of a link is a real authenticated raft `Ping` (cluster-key
//! handshake, one framed request, one framed reply), the exact exchange
//! heartbeats use, not a bare TCP connect: a peer whose listener still accepts
//! sockets while its raft path is wedged must read as down. The api half is a
//! TCP connect to the peer's web port. Both are on the internal network. The
//! vxlan overlay and drive-sync links are not probed separately.
//!
//! A reply to `Ping` proves the network and the listener, not the disk, yet a
//! raft follower has to fsync every append before it can answer the leader. So
//! each node also reports how long a small durable write takes on its own data
//! dir (`disk_ms`); a node with a stalled disk is reachable but cannot follow.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use cl_config::ClusterConfig;
use cl_raft::network::{ping, Peer};
use cl_raft::KeyWatch;
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

/// Upper bound on the durable write probe. A disk this slow is already
/// reported as such; waiting longer would only stall the whole report.
const DISK_PROBE_TIMEOUT: Duration = Duration::from_secs(8);

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
    /// Milliseconds a small write plus `fsync` took in this node's data dir;
    /// None when the probe failed or timed out.
    #[serde(default)]
    pub disk_ms: Option<u64>,
}

/// One node in the aggregate: `reported` is false when the node could not be
/// asked, in which case `links` is empty and says nothing about its peers.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct NodeReport {
    pub node: String,
    pub reported: bool,
    pub links: Vec<PeerLink>,
    /// See [`NodeLinks::disk_ms`]. None when the node did not report or the
    /// probe failed.
    pub disk_ms: Option<u64>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Connectivity {
    pub reports: Vec<NodeReport>,
}

/// A raft `Ping` over a fresh authenticated connection, timed end to end, so
/// the figure includes the handshake a reconnecting heartbeat would pay.
async fn probe_raft(addr: SocketAddr, keys: &KeyWatch) -> Option<u64> {
    let peer = Peer::connect(&addr.to_string(), keys.clone());
    let start = Instant::now();
    match tokio::time::timeout(PROBE_TIMEOUT, ping(&peer)).await {
        Ok(true) => Some(start.elapsed().as_millis() as u64),
        _ => None,
    }
}

/// Time one durable write in `dir`, the cost every raft append pays.
async fn probe_disk(dir: &std::path::Path) -> Option<u64> {
    let path = dir.join(".disk-probe");
    let work = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let start = Instant::now();
        let mut file = std::fs::File::create(&path).ok()?;
        file.write_all(&[0u8; 4096]).ok()?;
        file.sync_all().ok()?;
        drop(file);
        let _ = std::fs::remove_file(&path);
        Some(start.elapsed().as_millis() as u64)
    });
    tokio::time::timeout(DISK_PROBE_TIMEOUT, work)
        .await
        .ok()?
        .ok()?
}

async fn probe(addr: SocketAddr) -> Option<u64> {
    let start = Instant::now();
    match tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(addr)).await {
        Ok(Ok(_)) => Some(start.elapsed().as_millis() as u64),
        _ => None,
    }
}

/// Probe every other configured node from here.
pub(crate) async fn local_links(
    self_name: &str,
    cfg: &ClusterConfig,
    keys: &KeyWatch,
    data_dir: &std::path::Path,
) -> NodeLinks {
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
                            Some(a) => probe_raft(a, keys).await,
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
    let (links, disk_ms) =
        futures::future::join(futures::future::join_all(probes), probe_disk(data_dir)).await;
    NodeLinks {
        node: self_name.to_string(),
        links,
        disk_ms,
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
            let report = if *name == state.node_name {
                Some(local_links(name, cfg, &state.key_tx.subscribe(), &state.data_dir).await)
            } else if unreachable.contains(name) {
                // Already known down: do not pay the fetch timeout on every poll.
                None
            } else {
                match internal_peer::fetch_json::<NodeLinks>(state, name, INTERNAL_LINKS_PATH).await
                {
                    Ok(report) => Some(report),
                    Err(e) => {
                        warn!(node = %name, error = %e, "could not fetch a connectivity report");
                        None
                    }
                }
            };
            NodeReport {
                node: name.clone(),
                reported: report.is_some(),
                disk_ms: report.as_ref().and_then(|r| r.disk_ms),
                links: report.map(|r| r.links).unwrap_or_default(),
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
    Json(local_links(&state.node_name, &cfg, &state.key_tx.subscribe(), &state.data_dir).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_disk_probe_times_a_durable_write_and_cleans_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(probe_disk(dir.path()).await.is_some());
        assert!(!dir.path().join(".disk-probe").exists());
    }

    #[tokio::test]
    async fn the_disk_probe_reports_none_for_a_missing_dir() {
        assert_eq!(probe_disk(std::path::Path::new("/nonexistent/clouduler")).await, None);
    }

    #[tokio::test]
    async fn a_listener_that_accepts_but_never_speaks_reads_as_down() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _held = listener.accept().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let keys = cl_raft::throwaway_key();
        assert_eq!(probe_raft(addr, &keys).await, None);
        // The old bare connect would have called this link healthy.
        assert!(probe(addr).await.is_some());
    }
}
