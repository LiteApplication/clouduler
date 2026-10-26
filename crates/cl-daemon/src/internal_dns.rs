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

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use cl_config::{qualify, ClusterConfig, NetworkKind};
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

pub fn start(
    cfg: &ClusterConfig,
    cluster: &Arc<ClusterNode>,
    node_name: &str,
) -> Option<tokio::task::JoinHandle<()>> {
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
    let node_name = node_name.to_string();
    Some(tokio::spawn(async move {
        run(cluster, dns, boot_cfg, node_name).await;
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

async fn run(
    cluster: Arc<ClusterNode>,
    dns: Arc<InternalDns>,
    boot_cfg: ClusterConfig,
    node_name: String,
) {
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

        let records = zone_for(&state, &live, &nodes, cluster.quorum().leader());
        if let Err(e) = dns.reload(&records).await {
            warn!(error = %e, "could not rebuild the internal zone");
            continue;
        }
        let overrides = proxy_overrides(&state, &live, &node_name);
        if let Err(e) = dns.reload_overrides(&overrides).await {
            warn!(error = %e, "could not rebuild the proxy host overrides");
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
/// name, every node by its own name, plus every proxy hostname routed on
/// this same network (e.g. `mail` for stalwart's `mail.home.liteapp.fr`
/// route) so a container can reach another service the same way an outside
/// client does, not just by its internal `<service>.<stack>` name. Proxy
/// hosts routed on a *different* network are left out: their domain isn't
/// this zone's origin, so they belong to whatever resolves that zone
/// upstream, not to us.
///
/// The dashboard's own hostname (`web.address`/`web.network`) is included
/// too, resolved to the current leader alone - same rule `desired_records`
/// in `cl_daemon::proxy` already applies for the *external* DNS reconcile,
/// and for the same reason: writes only land on the leader
/// (`redirect_non_leader` in `cl_api::server`), so every other node's
/// address is a dead end for anything but a read. Without this, a
/// container has no way to resolve the dashboard's hostname at all: it
/// isn't a stack route, so `desired_dns_hosts` never sees it, and the
/// internal zone is authoritative for its own domain - a query for a name
/// under it that isn't in the zone comes back NXDOMAIN rather than falling
/// through to the upstream forwarder.
fn zone_for(
    state: &ClusterState,
    cfg: &ClusterConfig,
    nodes: &[(String, Ipv4Addr)],
    leader_id: Option<cl_raft::NodeId>,
) -> Vec<ZoneRecord> {
    let instances = crate::placement::instance_addresses(state, cfg);
    let mut records = build_records(
        instances.iter().map(|(id, ip)| (id, *ip)),
        nodes.iter().map(|(name, ip)| (name.as_str(), *ip)),
    );

    let node_ips: BTreeMap<&str, Ipv4Addr> = nodes
        .iter()
        .map(|(name, ip)| (name.as_str(), *ip))
        .collect();
    let (internal_name, _) = cfg.internal_network();
    for ((host, network), node_names) in crate::proxy::desired_dns_hosts(cfg, state) {
        if network != *internal_name {
            continue;
        }
        let mut addresses: Vec<Ipv4Addr> = node_names
            .iter()
            .filter_map(|n| node_ips.get(n.as_str()).copied())
            .collect();
        addresses.sort();
        addresses.dedup();
        if !addresses.is_empty() {
            records.push(ZoneRecord {
                name: host,
                addresses,
            });
        }
    }

    if cfg.web.network == *internal_name {
        if let Some(leader_ip) = leader_id.and_then(|id| {
            cfg.nodes
                .iter()
                .find(|(_, node)| cl_raft::node_id(node.id) == id)
                .and_then(|(name, _)| node_ips.get(name.as_str()).copied())
        }) {
            records.push(ZoneRecord {
                name: cfg.web.address.clone(),
                addresses: vec![leader_ip],
            });
        }
    }

    records.sort_by(|a, b| a.name.cmp(&b.name));
    records
}

/// Hostnames of routes published on a public network, pointed at this node's
/// own proxy over its internal address.
///
/// Resolved the normal way, such a name gives the node's public address, which
/// is usually the router's: a container on the node that asks for a hostname
/// the cluster itself serves then connects out to the router and back in, and
/// that never works from the same node (and only by luck from another). The
/// proxy on this node serves every route and relays to wherever the backend
/// runs, so answering with it is the same service the outside world gets.
///
/// Public networks only: they carry no network gate, so any container is
/// allowed. A route on a subnetted network is only served to peers inside
/// that subnet, which the resolver cannot tell per caller, so those names are
/// left to the upstream. Only done for the standard 80/443 proxy ports, since
/// a client given a bare hostname will use those. No AAAA is served for them
/// (the overlay has no IPv6 path to the proxy).
fn proxy_overrides(state: &ClusterState, cfg: &ClusterConfig, node_name: &str) -> Vec<ZoneRecord> {
    if !cfg.proxy.enabled || cfg.proxy.http_port != 80 || cfg.proxy.https_port != 443 {
        return Vec::new();
    }
    let Some(own_ip) = cfg
        .nodes
        .get(node_name)
        .and_then(|node| match cfg.internal_ip(node) {
            Some(IpAddr::V4(ip)) => Some(ip),
            _ => None,
        })
    else {
        return Vec::new();
    };
    let (_, internal) = cfg.internal_network();
    let internal_domain = internal.domain.as_deref().unwrap_or("");

    let mut names: BTreeSet<String> = BTreeSet::new();
    for (host, network) in crate::proxy::desired_dns_hosts(cfg, state).into_keys() {
        let Some(net) = cfg.networks.get(&network) else {
            continue;
        };
        if net.kind != NetworkKind::Public {
            continue;
        }
        let fqdn = qualify(&host, net.domain.as_deref());
        // The apex of the internal zone and anything under it is answered by
        // the zone itself.
        if !internal_domain.is_empty()
            && (fqdn == internal_domain || fqdn.ends_with(&format!(".{internal_domain}")))
        {
            continue;
        }
        names.insert(format!("{fqdn}."));
    }
    names
        .into_iter()
        .map(|name| ZoneRecord {
            name,
            addresses: vec![own_ip],
        })
        .collect()
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

    /// A proxy `host:` alias routed on the same network as the internal zone
    /// (SPEC's `blog` route is on `wan`, a different network, so it must stay
    /// absent) is not what a container asking for it by its public name
    /// should get: without this, only `<service>.<stack>` names resolve
    /// inside a container, and a health check hitting another service by the
    /// hostname it's actually published under (same as an outside client
    /// would use) gets NXDOMAIN.
    #[test]
    fn a_proxy_host_on_the_internal_network_resolves() {
        let spec = "\
networks:
  default: {}
services:
  web:
    image: nginx
    networks: [default]
    x-clouduler:
      proxy:
        - host: blog
          network: default
          port: 80
";
        let state = state_with(spec, 1);
        let nodes = vec![("n1".to_string(), Ipv4Addr::new(10, 10, 0, 1))];
        let records = zone_for(&state, &cluster_config(), &nodes, None);

        assert_eq!(
            answer(&records, "blog"),
            Some(&[Ipv4Addr::new(10, 10, 0, 1)][..])
        );
    }

    /// SPEC's own route is on `wan`, not the internal `default` network, so
    /// it must not leak into this zone: `wan` has its own domain, resolved
    /// by whatever answers for it, not by us.
    #[test]
    fn a_proxy_host_on_a_different_network_is_absent() {
        let state = state_with(SPEC, 1);
        let nodes = vec![("n1".to_string(), Ipv4Addr::new(10, 10, 0, 1))];
        let records = zone_for(&state, &cluster_config(), &nodes, None);

        assert_eq!(answer(&records, "blog"), None);
    }

    #[test]
    fn a_service_resolves_to_every_replica() {
        let state = state_with(SPEC, 2);
        let records = zone_for(&state, &cluster_config(), &[], None);

        let addrs = answer(&records, "web.blog").expect("web.blog resolves");
        assert_eq!(addrs.len(), 2);
        assert_eq!(answer(&records, "tasks.web.blog"), Some(addrs));
    }

    /// A container asking for a node by name is how it reaches a drive's
    /// endpoint without being told the topology.
    #[test]
    fn nodes_resolve_by_name() {
        let nodes = vec![("n1".to_string(), Ipv4Addr::new(10, 10, 0, 1))];
        let records = zone_for(&state_with(SPEC, 1), &cluster_config(), &nodes, None);
        assert_eq!(
            answer(&records, "n1"),
            Some(&[Ipv4Addr::new(10, 10, 0, 1)][..])
        );
    }

    /// `nodes_of` is recomputed every reload pass from whatever config is
    /// currently live (see `run`), so a node added after this one started is
    /// picked up without a restart.
    #[test]
    fn nodes_of_reflects_a_newly_added_node() {
        let cfg = ClusterConfig::parse(include_str!("../../../examples/cluster.yaml"))
            .expect("example parses");
        let mut without_n3 = cfg.clone();
        without_n3.nodes.remove("n3");

        assert!(!nodes_of(&without_n3).iter().any(|(name, _)| name == "n3"));
        assert!(nodes_of(&cfg)
            .iter()
            .any(|(name, ip)| name == "n3" && *ip == Ipv4Addr::new(10, 10, 0, 3)));
    }

    fn public_cfg(proxy_ports: &str) -> ClusterConfig {
        ClusterConfig::parse(&format!(
            "\
cluster: {{ name: test }}
networks:
  default: {{ type: internal, subnet: 10.10.0.0/24, domain: int.example.com }}
  pub: {{ type: public, domain: example.org }}
nodes:
  n1:
    id: 3b0f6c4a-3d3f-4a8e-9b8b-2c1e6a2a9f01
    role: manager
    network: {{ default: 10.10.0.1, pub: {{ ipv4: 203.0.113.5 }} }}
web: {{ address: cloud, network: default }}
{proxy_ports}"
        ))
        .expect("valid cluster config")
    }

    const PUBLIC_SPEC: &str = "\
networks:
  default: {}
services:
  web:
    image: nginx
    networks: [default]
    x-clouduler:
      proxy:
        - host: auth
          network: pub
          port: 80
";

    /// A public route's name resolves to this node's own proxy, not to the
    /// router address the outside world gets, which a container on the same
    /// node cannot connect to.
    #[test]
    fn a_public_route_resolves_to_this_nodes_proxy() {
        let records = proxy_overrides(&state_with(PUBLIC_SPEC, 1), &public_cfg(""), "n1");
        assert_eq!(
            records,
            vec![ZoneRecord {
                name: "auth.example.org.".to_string(),
                addresses: vec![Ipv4Addr::new(10, 10, 0, 1)],
            }]
        );
    }

    /// SPEC's route sits on `wan`, an egress network: not a public one, so
    /// nothing is overridden for it.
    #[test]
    fn a_route_on_a_non_public_network_is_left_to_the_upstream() {
        let state = state_with(SPEC, 1);
        assert!(proxy_overrides(&state, &cluster_config(), "n1").is_empty());
    }

    #[test]
    fn nothing_is_overridden_when_the_proxy_is_off_or_on_other_ports() {
        let state = state_with(PUBLIC_SPEC, 1);
        let off = public_cfg("proxy: { enabled: false }");
        assert!(proxy_overrides(&state, &off, "n1").is_empty());
        let odd = public_cfg("proxy: { https_port: 9443 }");
        assert!(proxy_overrides(&state, &odd, "n1").is_empty());
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

    /// The dashboard hostname (`web.address`) isn't a stack route, so
    /// nothing else in `zone_for` would ever produce a record for it - a
    /// container asking for it before this existed got NXDOMAIN even though
    /// the cluster answers for the exact same domain externally.
    #[test]
    fn the_dashboard_hostname_resolves_to_the_leader() {
        let nodes = vec![("n1".to_string(), Ipv4Addr::new(10, 10, 0, 1))];
        let leader = cl_raft::node_id(
            uuid::Uuid::parse_str("3b0f6c4a-3d3f-4a8e-9b8b-2c1e6a2a9f01").unwrap(),
        );
        let records = zone_for(
            &state_with(SPEC, 1),
            &cluster_config(),
            &nodes,
            Some(leader),
        );
        assert_eq!(
            answer(&records, "cloud"),
            Some(&[Ipv4Addr::new(10, 10, 0, 1)][..])
        );
    }

    /// No known leader (an election in flight) means no guess: the hostname
    /// is simply absent rather than pointing at a node that might not even
    /// be caught up, matching the external DNS reconcile's own rule.
    #[test]
    fn the_dashboard_hostname_is_absent_with_no_leader() {
        let nodes = vec![("n1".to_string(), Ipv4Addr::new(10, 10, 0, 1))];
        let records = zone_for(&state_with(SPEC, 1), &cluster_config(), &nodes, None);
        assert_eq!(answer(&records, "cloud"), None);
    }
}
