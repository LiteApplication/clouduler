//! Wiring the container relay (`cl_net::relay`) into the daemon.
//!
//! Two halves. The listener, on this node's internal address at
//! `relay_port(rpc_port)`, lets the other nodes reach the containers this
//! node runs. The `Dialer` is this node's own way out: the reverse proxy, its
//! health checks and the expose forwarders dial every container through it,
//! and its relay table - which containers run on which other node - is
//! refreshed here from replicated placement.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use cl_config::ClusterConfig;
use cl_net::relay::{relay_port, serve, Dialer};
use cl_raft::node::ClusterNode;
use cl_raft::KeyWatch;
use ipnet::IpNet;
use tokio::net::TcpListener;
use tracing::{info, warn};

/// How often the relay table is rebuilt. Matches the scheduler's own sweep,
/// since that is what moves a container.
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// How long the listener waits before binding again, usually because the
/// internal address is not up yet.
const BIND_RETRY: Duration = Duration::from_secs(10);

pub struct Relay {
    /// What every host-side connection to a container goes through.
    pub dialer: Dialer,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Relay {
    pub fn abort(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

pub fn start(
    cfg: &ClusterConfig,
    node_name: &str,
    cluster: &Arc<ClusterNode>,
    keys: KeyWatch,
) -> Relay {
    let dialer = Dialer::new(keys.clone());
    let mut tasks = Vec::new();

    match cfg.node(node_name).and_then(|node| {
        Some(SocketAddr::new(
            cfg.internal_ip(node)?,
            relay_port(node.rpc_port),
        ))
    }) {
        Some(addr) => tasks.push(tokio::spawn(listen(addr, keys))),
        // The other nodes then cannot reach this node's containers from the
        // host side, but nothing else here depends on it.
        None => warn!("this node has no internal address, not serving the container relay"),
    }

    let refreshed = dialer.clone();
    let cluster = cluster.clone();
    let boot_cfg = cfg.clone();
    let node_name = node_name.to_string();
    tasks.push(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(REFRESH_INTERVAL);
        loop {
            ticker.tick().await;
            let state = cluster.state().await;
            let live = crate::daemon::live_config(&state, &boot_cfg);
            refreshed.set_relays(crate::placement::relay_routes(&state, &live, &node_name));
        }
    }));

    Relay { dialer, tasks }
}

/// Serve the relay on `addr` for as long as the process lives, binding again
/// whenever the listener stops.
async fn listen(addr: SocketAddr, keys: KeyWatch) {
    // Only container addresses are ever dialed for a peer: the relay is a
    // way to the containers this node runs, not into the host or onward.
    let allowed = vec![IpNet::V4(cl_net::egress::overlay_pool())];
    loop {
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                info!(%addr, "container relay listening");
                if let Err(e) = serve(listener, keys.clone(), allowed.clone()).await {
                    warn!(%addr, error = %e, "container relay stopped, retrying");
                }
            }
            Err(e) => warn!(%addr, error = %e, "cannot bind the container relay, retrying"),
        }
        tokio::time::sleep(BIND_RETRY).await;
    }
}
