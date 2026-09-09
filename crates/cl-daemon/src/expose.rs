//! Published non-HTTP ports (`x-clouduler.expose`).
//!
//! Every node listens on every published port, and forwards to whichever node
//! currently runs the container, over the overlay. That means a published port
//! answers on any node's address, so the DNS record in front of it does not
//! have to be rewritten every time a container moves.
//!
//! The forwarders are started once per published port and left running; only
//! the backend behind them changes as placement does, which is what keeps an
//! open connection from surviving a move it should not survive.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use cl_config::{ClusterConfig, Protocol};
use cl_net::expose::{serve_tcp, serve_udp, Routes};
use cl_raft::node::ClusterNode;
use cl_raft::state::ClusterState;
use tracing::{info, warn};

/// How often placement is re-read. Matches the scheduler's own sweep, since
/// that is what moves a backend.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// How long a forwarder waits before binding again after its listener
/// stopped, usually because something else already holds the port.
const BIND_RETRY: Duration = Duration::from_secs(30);

pub fn start(cfg: &ClusterConfig, cluster: &Arc<ClusterNode>) -> tokio::task::JoinHandle<()> {
    let cfg = cfg.clone();
    let cluster = cluster.clone();
    tokio::spawn(async move { run(cluster, cfg).await })
}

async fn run(cluster: Arc<ClusterNode>, boot_cfg: ClusterConfig) {
    let routes = Routes::new();
    // Keyed by port, protocol *and* proxy-protocol setting: see the loop body.
    let mut listening: BTreeMap<(u16, Protocol, bool), tokio::task::JoinHandle<()>> = BTreeMap::new();
    let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);

    loop {
        ticker.tick().await;
        let state = cluster.state().await;
        // Recomputed every pass like `internal_dns`'s own live config, so a
        // network's kind changing (or a node joining) is picked up without a
        // restart.
        let live = crate::daemon::live_config(&state, &boot_cfg);
        let desired = desired(&state, &live);

        for ((published, protocol), publish) in &desired {
            routes.set(*published, publish.backend);
            // Keyed on `proxy_protocol` as well as the port, so turning it on
            // or off in a stack file takes effect on the next sweep: the old
            // listener is dropped by the retain below and a new one binds
            // with the new behaviour. Only the backend address is live-updated
            // through `routes`; this is baked in when the listener starts.
            if listening.contains_key(&(*published, *protocol, publish.proxy_protocol)) {
                continue;
            }

            let bind = SocketAddr::from(([0, 0, 0, 0], *published));
            let routes = routes.clone();
            let published = *published;
            let protocol = *protocol;
            let proxy_protocol = publish.proxy_protocol;
            listening.insert(
                (published, protocol, proxy_protocol),
                // Retried from inside the task: something else may hold the
                // port for a while, and a forwarder that gave up for good
                // would leave the port unpublished until the daemon restarts.
                tokio::spawn(async move {
                    loop {
                        let served = match protocol {
                            Protocol::Tcp => serve_tcp(bind, published, routes.clone(), proxy_protocol).await,
                            Protocol::Udp => serve_udp(bind, published, routes.clone()).await,
                        };
                        if let Err(e) = served {
                            warn!(%bind, %protocol, error = %e, "published port stopped");
                        }
                        tokio::time::sleep(BIND_RETRY).await;
                    }
                }),
            );
            info!(%bind, %protocol, proxy_protocol, "publishing a port");
        }

        // A port nobody publishes any more stops being listened on, or a
        // later stack could never claim it.
        listening.retain(|key, task| {
            let (published, protocol, proxy_protocol) = *key;
            // Matched on the flag too: a listener still serving the port but
            // with the wrong proxy-protocol behaviour has already been
            // replaced above, and has to go.
            if desired
                .get(&(published, protocol))
                .is_some_and(|publish| publish.proxy_protocol == proxy_protocol)
            {
                return true;
            }
            task.abort();
            info!(published, protocol = %protocol, "stopped publishing a port");
            false
        });
        for (published, _, _) in listening.keys() {
            if !desired.keys().any(|(p, _)| p == published) {
                routes.set(*published, None);
            }
        }
    }
}

/// One published port: where it forwards, and how it announces the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Publish {
    /// `None` means published but nothing running behind it right now.
    backend: Option<SocketAddr>,
    proxy_protocol: bool,
}

/// Published ports and the backend behind each, from replicated state alone.
///
/// A service with several replicas is forwarded to the first of them rather
/// than balanced: these are raw connections with no notion of a request to
/// spread, and picking the lowest instance keeps every node choosing the same
/// one. `None` means the port is published but nothing is running behind it,
/// which the forwarder answers by refusing the connection rather than hanging.
fn desired(state: &ClusterState, cfg: &ClusterConfig) -> BTreeMap<(u16, Protocol), Publish> {
    let mut out: BTreeMap<(u16, Protocol), Publish> = BTreeMap::new();

    for placement in crate::placement::service_placements(state, cfg) {
        for entry in &placement.extensions.expose {
            let key = (entry.published, entry.protocol);
            let backend = Publish {
                backend: placement
                    .addresses
                    .first()
                    .map(|ip| SocketAddr::from((*ip, entry.port))),
                // Ignored for UDP, which has no stream to put a header at the
                // front of. The stack validator rejects the combination, so
                // reaching here with both set means an older stack stored
                // before that check existed.
                proxy_protocol: entry.proxy_protocol && entry.protocol == Protocol::Tcp,
            };

            match out.get(&key) {
                // Two services claiming one published port is a config the
                // validator does not catch across stacks. Keeping the first
                // by name at least makes every node agree which one wins.
                Some(_) => warn!(
                    published = entry.published,
                    stack = %placement.stack,
                    service = %placement.service,
                    "published port is already claimed, ignoring this one"
                ),
                None => {
                    out.insert(key, backend);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placement::fixtures::{cluster_config, state_with, SPEC};
    use cl_raft::ids::InstanceId;

    #[test]
    fn an_exposed_port_forwards_to_the_container_port() {
        let desired = desired(&state_with(SPEC, 1), &cluster_config());
        let backend = desired[&(2222, Protocol::Tcp)].backend.expect("a backend");
        assert_eq!(backend.port(), 22);
    }

    /// Losing the container must clear the route rather than leave it
    /// pointing at an address nothing answers on.
    #[test]
    fn a_port_with_nothing_running_keeps_listening_with_no_backend() {
        let mut state = state_with(SPEC, 1);
        state.assignments.remove(&InstanceId::new("blog", "web", 0));

        let desired = desired(&state, &cluster_config());
        assert!(desired.contains_key(&(2222, Protocol::Tcp)));
        assert_eq!(desired[&(2222, Protocol::Tcp)].backend, None);
    }

    /// Every node picks the same replica, or a client reconnecting to a
    /// different node lands somewhere else for no reason.
    #[test]
    fn replicas_are_picked_deterministically() {
        let state = state_with(SPEC, 3);
        let first = desired(&state, &cluster_config());
        assert_eq!(first, desired(&state, &cluster_config()));
    }

    #[test]
    fn a_stack_exposing_nothing_publishes_nothing() {
        let spec = "networks:\n  default: {}\nservices:\n  web:\n    image: nginx\n";
        assert!(desired(&state_with(spec, 1), &cluster_config()).is_empty());
    }
}
