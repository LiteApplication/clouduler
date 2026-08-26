//! Wiring the internal resolver into the daemon.
//!
//! Containers get one resolver: the gateway address of their own overlay
//! network, which is an address on this node's bridge. It answers
//! `<service>.<stack>` and `tasks.<service>.<stack>` for the whole cluster
//! from replicated state, and forwards everything else to whatever this node
//! resolves with.
//!
//! Binding per network rather than once on all addresses is deliberate: the
//! gateway addresses belong to clouduler, so nothing else on the node is
//! listening there, and port 53 on the node's own addresses stays free for
//! whatever the operator runs.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use cl_config::ClusterConfig;
use cl_net::dns::{build_records, zone_origin, InternalDns, ZoneRecord};
use cl_net::Allocations;
use cl_raft::node::ClusterNode;
use cl_raft::state::ClusterState;
use hickory_proto::rr::Name;
use tracing::{debug, info, warn};

/// How often the zone is rebuilt. Records carry a five second TTL, so a
/// slower pass than this would leave a moved service unreachable for longer
/// than the TTL suggests.
const RELOAD_INTERVAL: Duration = Duration::from_secs(5);

/// Standard DNS. Not configurable: it is what a container's resolver library
/// asks on, and the address it is bound to is ours alone.
const DNS_PORT: u16 = 53;

/// How long to wait before trying a bind again. Long enough that a node which
/// will never hold this address is not retrying in a tight loop.
const BIND_RETRY: Duration = Duration::from_secs(30);

pub fn start(cfg: &ClusterConfig, cluster: &Arc<ClusterNode>) -> Option<tokio::task::JoinHandle<()>> {
    let origin = zone_origin(cfg);
    let name = match Name::parse(&origin, Some(&Name::root())) {
        Ok(name) => name,
        Err(e) => {
            warn!(%origin, error = %e, "not a usable dns zone, containers will not resolve each other");
            return None;
        }
    };

    let dns = match InternalDns::new(name, &[]) {
        Ok(dns) => Arc::new(dns),
        Err(e) => {
            warn!(%origin, error = %e, "cannot build the internal zone");
            return None;
        }
    };

    info!(zone = %origin, "internal dns ready");
    let cluster = cluster.clone();
    let boot_cfg = cfg.clone();
    Some(tokio::spawn(async move {
        run(cluster, dns, boot_cfg).await;
    }))
}

/// Every node's address on the internal network, by name.
fn nodes_of(cfg: &ClusterConfig) -> Vec<(String, Ipv4Addr)> {
    cfg.nodes
        .iter()
        .filter_map(|(name, node)| match cfg.internal_ip(node) {
            Some(IpAddr::V4(ip)) => Some((name.clone(), ip)),
            _ => None,
        })
        .collect()
}

async fn run(cluster: Arc<ClusterNode>, dns: Arc<InternalDns>, boot_cfg: ClusterConfig) {
    // Gateways already being served. A bind that failed is not recorded, so
    // the next pass retries it: the address only exists once the overlay loop
    // has created the bridge, which may be a tick or two away.
    let mut bound: BTreeSet<Ipv4Addr> = BTreeSet::new();
    let mut ticker = tokio::time::interval(RELOAD_INTERVAL);

    loop {
        ticker.tick().await;
        let state = cluster.state().await;
        // Recomputed every pass, not just once at boot, so a node added after
        // this one started gets an internal-dns answer without a restart.
        let live = crate::daemon::live_config(&state, &boot_cfg);
        let nodes = nodes_of(&live);

        let records = zone_for(&state, &live, &nodes);
        if let Err(e) = dns.reload(&records).await {
            warn!(error = %e, "could not rebuild the internal zone");
            continue;
        }

        for gateway in gateways(&state) {
            if !bound.insert(gateway) {
                continue;
            }
            let addr = SocketAddr::from((gateway, DNS_PORT));
            let dns = dns.clone();
            // The bind retries rather than being retried from out here: the
            // address only exists once the overlay loop has built the bridge,
            // which can be several passes away on a node that has just been
            // given its first container on this network.
            tokio::spawn(async move {
                loop {
                    match dns.serve(addr).await {
                        Ok(()) => return,
                        Err(e) => debug!(%addr, error = %e, "internal dns not serving here yet"),
                    }
                    tokio::time::sleep(BIND_RETRY).await;
                }
            });
            // Logged as an attempt, not a success: the bind is what decides,
            // and it happens inside the task above.
            info!(%addr, "binding the internal resolver");
        }
    }
}

/// The zone as it should be right now: every running container by service
/// name, plus every node by its own name.
fn zone_for(state: &ClusterState, cfg: &ClusterConfig, nodes: &[(String, Ipv4Addr)]) -> Vec<ZoneRecord> {
    let instances = crate::placement::instance_addresses(state, cfg);
    build_records(
        instances.iter().map(|(id, ip)| (id, *ip)),
        nodes.iter().map(|(name, ip)| (name.as_str(), *ip)),
    )
}

/// Gateway address of every overlay network that has a subnet. Bound
/// everywhere rather than only on networks with a container here, because a
/// container can be started on this node between two passes and its resolver
/// has to answer immediately.
fn gateways(state: &ClusterState) -> Vec<Ipv4Addr> {
    Allocations::decode(state.overlay.as_deref())
        .map(|a| a.gateways().map(|(_, ip)| ip).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placement::fixtures::{cluster_config, state_with, SPEC};
    use cl_net::dns::answer;

    #[test]
    fn a_service_resolves_to_every_replica() {
        let state = state_with(SPEC, 2);
        let records = zone_for(&state, &cluster_config(), &[]);

        let addrs = answer(&records, "web.blog").expect("web.blog resolves");
        assert_eq!(addrs.len(), 2);
        assert_eq!(answer(&records, "tasks.web.blog"), Some(addrs));
    }

    /// A container asking for a node by name is how it reaches a drive's
    /// endpoint without being told the topology.
    #[test]
    fn nodes_resolve_by_name() {
        let nodes = vec![("n1".to_string(), Ipv4Addr::new(10, 10, 0, 1))];
        let records = zone_for(&state_with(SPEC, 1), &cluster_config(), &nodes);
        assert_eq!(answer(&records, "n1"), Some(&[Ipv4Addr::new(10, 10, 0, 1)][..]));
    }

    /// `nodes_of` is recomputed every reload pass from whatever config is
    /// currently live (see `run`), so a node added after this one started is
    /// picked up without a restart.
    #[test]
    fn nodes_of_reflects_a_newly_added_node() {
        let cfg = ClusterConfig::parse(include_str!("../../../examples/cluster.yaml")).expect("example parses");
        let mut without_n3 = cfg.clone();
        without_n3.nodes.remove("n3");

        assert!(!nodes_of(&without_n3).iter().any(|(name, _)| name == "n3"));
        assert!(nodes_of(&cfg).iter().any(|(name, ip)| name == "n3" && *ip == Ipv4Addr::new(10, 10, 0, 3)));
    }

    #[test]
    fn a_network_with_a_subnet_gets_a_resolver_address() {
        let state = state_with(SPEC, 1);
        let gateways = gateways(&state);
        assert_eq!(gateways.len(), 1);
        // First host address of the allocated subnet, which is what the
        // bridge holds and what containers are handed as their resolver.
        assert!(gateways[0].to_string().ends_with(".1"));
    }

    #[test]
    fn a_cluster_with_no_overlay_serves_nothing() {
        assert!(gateways(&ClusterState::default()).is_empty());
    }
}
