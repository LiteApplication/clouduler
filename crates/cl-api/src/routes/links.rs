//! `/api/nodes/connectivity`: who can reach whom.
//!
//! Every node probes its peers and reports what *it* sees, so a one-way break shows as two different
//! reports instead of being averaged away. The aggregate asks every node over the signed channel
//! (`internal_peer`); a node that cannot be asked is reported as having no report.
//!
//! Each probe round is also recorded, every `LINK_PROBE_INTERVAL` by a background task
//! (`spawn_link_probes`), into `AppState::link_health`, a rolling history of the last
//! `cl_sched::link_health::HISTORY_LEN` rounds per peer. A single snapshot hides a link that drops
//! for seconds at a time; the history yields a `state` of `ok`, `flapping` or `down` per link, and the
//! scheduler reads the same history to avoid crash-loop moves while links are unstable.
//!
//! The raft half is a real authenticated raft `Ping` (the exchange heartbeats use), not a bare TCP
//! connect: a peer whose listener accepts sockets while its raft path is wedged must read as down. The
//! api half is a TCP connect to the web port and the drive half an authenticated connect to the
//! drive-sync port (`cl_drive::drive_port` of the raft port). The vxlan overlay is not probed.
//!
//! A `Ping` reply proves the network and listener, not the disk, yet a follower must fsync every
//! append, so each node also reports a small durable write time (`disk_ms`); a stalled disk is
//! reachable but cannot follow.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use cl_config::ClusterConfig;
use cl_raft::network::{ping, Peer};
use cl_raft::KeyWatch;
use cl_sched::{LinkHealth, LinkSample, LinkState};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tracing::warn;

use crate::auth::internal::InternalAuth;
use crate::auth::Identity;
use crate::error::ApiError;
use crate::internal_peer;
use crate::server::AppState;

/// Short enough that a probe round stays inside the 5s `internal_peer` fetch timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How often the background task probes every peer and records the result.
const LINK_PROBE_INTERVAL: Duration = Duration::from_secs(10);

const DISK_PROBE_TIMEOUT: Duration = Duration::from_secs(8);

const INTERNAL_LINKS_PATH: &str = "/api/nodes/internal/links";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/nodes/connectivity", get(connectivity))
        .route(INTERNAL_LINKS_PATH, get(internal_links))
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct PeerLink {
    pub peer: String,
    pub raft: bool,
    pub api: bool,
    /// The drive-sync port. A node older than this field is assumed up.
    #[serde(default = "assume_up")]
    pub drive: bool,
    pub raft_ms: Option<u64>,
    /// `ok`, `flapping` or `down`, from `history` (not from this round alone).
    #[serde(default)]
    pub state: LinkState,
    /// Failed rounds in `history`; a round fails when raft, api or drive does not answer.
    #[serde(default)]
    pub failures: usize,
    /// The recorded rounds, oldest first, at most `cl_sched::link_health::HISTORY_LEN`.
    #[serde(default)]
    pub history: Vec<LinkSample>,
}

fn assume_up() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct NodeLinks {
    pub node: String,
    pub links: Vec<PeerLink>,
    #[serde(default)]
    pub disk_ms: Option<u64>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct NodeReport {
    pub node: String,
    pub reported: bool,
    pub links: Vec<PeerLink>,
    pub disk_ms: Option<u64>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Connectivity {
    pub reports: Vec<NodeReport>,
}

/// Timed end to end, so it includes the handshake a reconnecting heartbeat would pay.
async fn probe_raft(addr: SocketAddr, keys: &KeyWatch) -> Option<u64> {
    let peer = Peer::connect(&addr.to_string(), keys.clone());
    let start = Instant::now();
    match tokio::time::timeout(PROBE_TIMEOUT, ping(&peer)).await {
        Ok(true) => Some(start.elapsed().as_millis() as u64),
        _ => None,
    }
}

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

/// Authenticated, since the drive listener drops (and warns about) any connection that does not
/// prove the cluster key; a bare connect would log a failed authentication per probe.
async fn probe_drive(addr: SocketAddr, keys: &KeyWatch) -> Option<u64> {
    let start = Instant::now();
    match tokio::time::timeout(
        PROBE_TIMEOUT,
        cl_drive::transport::connect_authenticated(addr, keys),
    )
    .await
    {
        Ok(Ok(_)) => Some(start.elapsed().as_millis() as u64),
        _ => None,
    }
}

/// One probe round of every peer. The history fields are left empty for the caller to fill in.
async fn probe_peers(self_name: &str, cfg: &ClusterConfig, keys: &KeyWatch) -> Vec<PeerLink> {
    let probes = cfg
        .nodes
        .iter()
        .filter(|(name, _)| name.as_str() != self_name)
        .map(|(name, node)| {
            let raft_addr = cfg.rpc_addr(node);
            let api_addr = cfg
                .internal_ip(node)
                .map(|ip| SocketAddr::new(ip, cfg.web.port));
            let drive_addr =
                raft_addr.map(|a| SocketAddr::new(a.ip(), cl_drive::drive_port(a.port())));
            let peer = name.clone();
            // A node with no drives has nothing to sync, so its drive port is not probed.
            let has_drives = !node.drives.is_empty();
            async move {
                let (raft_ms, api_ms, drive_ms) = futures::future::join3(
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
                    async {
                        match drive_addr {
                            Some(a) => probe_drive(a, keys).await,
                            None => None,
                        }
                    },
                )
                .await;
                PeerLink {
                    peer,
                    raft: raft_ms.is_some(),
                    api: api_ms.is_some(),
                    drive: !has_drives || drive_ms.is_some(),
                    raft_ms,
                    state: LinkState::Ok,
                    failures: 0,
                    history: Vec::new(),
                }
            }
        });
    futures::future::join_all(probes).await
}

fn sample_of(link: &PeerLink, at: i64) -> LinkSample {
    LinkSample {
        at,
        raft: link.raft,
        api: link.api,
        drive: link.drive,
        raft_ms: link.raft_ms,
    }
}

/// Probes every peer each `LINK_PROBE_INTERVAL` and records the round, so the history exists
/// whether or not anyone is looking at the matrix.
pub fn spawn_link_probes(state: AppState) {
    let mut shutdown = state.shutdown.subscribe();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(LINK_PROBE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => return,
            }
            let cfg = state.cluster_config.read().await.clone();
            let keys = state.key_tx.subscribe();
            let links = probe_peers(&state.node_name, &cfg, &keys).await;
            let at = crate::alerts::unix_now();
            for link in &links {
                state.link_health.record(&link.peer, sample_of(link, at));
            }
            state
                .link_health
                .retain_peers(links.iter().map(|l| l.peer.as_str()));
        }
    });
}

pub(crate) async fn local_links(
    self_name: &str,
    cfg: &ClusterConfig,
    keys: &KeyWatch,
    data_dir: &std::path::Path,
    health: &LinkHealth,
) -> NodeLinks {
    let probes = probe_peers(self_name, cfg, keys);
    let disk_enabled = cfg.nodes.get(self_name).is_none_or(|n| n.disk_probe);
    let disk = async {
        if disk_enabled {
            probe_disk(data_dir).await
        } else {
            None
        }
    };
    let (mut links, disk_ms) = futures::future::join(probes, disk).await;
    for link in &mut links {
        let summary = health.summary(&link.peer);
        link.state = summary.state;
        link.failures = summary.failures;
        link.history = summary.history;
    }
    NodeLinks {
        node: self_name.to_string(),
        links,
        disk_ms,
    }
}

pub(crate) async fn collect(state: &AppState) -> Connectivity {
    let cfg = state.cluster_config.read().await.clone();
    let unreachable = state.cluster.state().await.unreachable;

    let asks = cfg.nodes.keys().map(|name| {
        let cfg = &cfg;
        let unreachable = &unreachable;
        async move {
            let report = if *name == state.node_name {
                Some(
                    local_links(
                        name,
                        cfg,
                        &state.key_tx.subscribe(),
                        &state.data_dir,
                        &state.link_health,
                    )
                    .await,
                )
            } else if unreachable.contains(name) {
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

async fn internal_links(_auth: InternalAuth, State(state): State<AppState>) -> Json<NodeLinks> {
    let cfg = state.cluster_config.read().await.clone();
    Json(
        local_links(
            &state.node_name,
            &cfg,
            &state.key_tx.subscribe(),
            &state.data_dir,
            &state.link_health,
        )
        .await,
    )
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
        assert_eq!(
            probe_disk(std::path::Path::new("/nonexistent/clouduler")).await,
            None
        );
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
        assert!(probe(addr).await.is_some());
    }
}
