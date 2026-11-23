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
    wan: Arc<crate::wan::WanFirewall>,
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

    dns.set_observer(wan);

    info!(zone = %origin, "internal dns ready");
    let cluster = cluster.clone();
    let boot_cfg = cfg.clone();
    let node_name = node_name.to_string();
    Some(tokio::spawn(async move {
        run(cluster, dns, boot_cfg, node_name).await;
    }))
}

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
        let state = cluster.snapshot().await;
        let live = crate::daemon::live_config(&state, &boot_cfg);
        let nodes = nodes_of(&live);

        let records = zone_for(&state, &live, &nodes, cluster.quorum().leader());
        if let Err(e) = dns.reload(&records).await {
            warn!(error = %e, "could not rebuild the internal zone");
            continue;
        }
        let views = zone_views(&state, &live, &records);
        if let Err(e) = dns.reload_views(&views).await {
            warn!(error = %e, "could not rebuild the per-container dns views");
        }
        let overrides = proxy_overrides(&state, &live, &node_name);
        let skip = nat_clients(&state, &live);
        if let Err(e) = dns.reload_overrides(&overrides, skip).await {
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
            info!(%addr, "binding the internal resolver");
        }
    }
}

/// The zone as it should be right now: every running container by service name,
/// every node by its own name, plus every proxy hostname routed on this same
/// network (so a container reaches another service by its public name too).
/// Hosts routed on a different network are left to the upstream resolver.
///
/// The dashboard hostname (`web.address`/`web.network`) is included, resolved to
/// the current leader alone, since writes only land there. It is not a stack
/// route, and the zone is authoritative for its domain, so without it the name
/// would come back NXDOMAIN instead of reaching the upstream forwarder.
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
    let Some((internal_name, _)) = cfg.internal_network() else {
        return records;
    };
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

/// Answers for the containers on managed networks that differ from the shared
/// zone. A service listed on a managed network has an address there beside the
/// one on its stack overlay, and an asker can only reach the one on a network it
/// shares: its own stack overlay first (the primary network), then its managed
/// networks. Each such asker, by every address it holds, gets the whole zone with
/// those service names answered accordingly. Empty without managed networks.
fn zone_views(
    state: &ClusterState,
    cfg: &ClusterConfig,
    shared: &[ZoneRecord],
) -> BTreeMap<IpAddr, Vec<ZoneRecord>> {
    use cl_raft::ids::InstanceId;
    let managed = crate::placement::managed_addresses(state, cfg);
    if managed.is_empty() {
        return BTreeMap::new();
    }
    let primary: BTreeMap<InstanceId, Ipv4Addr> = crate::placement::instance_addresses(state, cfg)
        .into_iter()
        .collect();

    // Every network an instance sits on, primary first: (network key, address).
    let networks_of = |id: &InstanceId| -> Vec<(String, Ipv4Addr)> {
        let mut out = Vec::new();
        let managed_of = managed.get(id).cloned().unwrap_or_default();
        if let Some(ip) = primary.get(id) {
            match managed_of.iter().find(|(_, m)| m == ip) {
                // No stack overlay: the primary network is the managed one.
                Some((network, _)) => out.push((format!("@{network}"), *ip)),
                None => out.push((format!("stack:{}", id.stack), *ip)),
            }
        }
        for (network, ip) in managed_of {
            if !out.iter().any(|(_, a)| *a == ip) {
                out.push((format!("@{network}"), ip));
            }
        }
        out
    };

    // The instances of each service, with their networks.
    type InstanceNets = Vec<(String, Ipv4Addr)>;
    let mut services: BTreeMap<(String, String), Vec<InstanceNets>> = BTreeMap::new();
    for id in primary.keys() {
        services
            .entry((id.service.clone(), id.stack.clone()))
            .or_default()
            .push(networks_of(id));
    }

    let mut views = BTreeMap::new();
    for asker in managed.keys() {
        let mine = networks_of(asker);
        let mut records = shared.to_vec();
        let mut differs = false;
        for ((service, stack), instances) in &services {
            let mut answer: Vec<Ipv4Addr> = instances
                .iter()
                .filter_map(|networks| {
                    mine.iter()
                        .find_map(|(key, _)| networks.iter().find(|(k, _)| k == key))
                        .or_else(|| networks.first())
                        .map(|(_, ip)| *ip)
                })
                .collect();
            answer.sort();
            answer.dedup();
            for name in [
                format!("{service}.{stack}"),
                format!("tasks.{service}.{stack}"),
            ] {
                if let Some(record) = records.iter_mut().find(|r| r.name == name) {
                    if record.addresses != answer {
                        record.addresses = answer.clone();
                        differs = true;
                    }
                }
            }
        }
        if differs {
            for (_, ip) in &mine {
                views.insert(IpAddr::V4(*ip), records.clone());
            }
        }
    }
    views
}

/// Hostnames of routes published on a public network, pointed at this node's
/// own proxy over its internal address.
///
/// Resolved normally such a name gives the router's public address, and a
/// container connecting out to it and back in never works from the same node.
/// Public networks only (no network gate); subnetted routes are left to the
/// upstream since the resolver cannot tell callers apart. Only for the standard
/// 80/443 proxy ports, and no AAAA (the overlay has no IPv6 path to the proxy).
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
    let internal_domain = cfg
        .internal_network()
        .and_then(|(_, net)| net.domain.as_deref())
        .unwrap_or("");

    let mut names: BTreeSet<String> = BTreeSet::new();
    for (host, network) in crate::proxy::desired_dns_hosts(cfg, state).into_keys() {
        let Some(net) = cfg.networks.get(&network) else {
            continue;
        };
        if net.kind != NetworkKind::Public {
            continue;
        }
        let fqdn = qualify(&host, net.domain.as_deref());
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

/// Containers of stacks that attach a nat network. They get no proxy
/// override: a name the cluster serves resolves to its real address for them,
/// as for any outside client, instead of this node's internal proxy.
fn nat_clients(state: &ClusterState, cfg: &ClusterConfig) -> BTreeSet<IpAddr> {
    let nat_stacks: BTreeSet<&str> = state
        .stacks
        .iter()
        .filter_map(|(name, stack)| {
            let file = cl_config::StackFile::parse(&stack.spec).ok()?;
            file.networks
                .keys()
                .any(|n| {
                    cfg.networks
                        .get(n)
                        .is_some_and(|net| net.kind == NetworkKind::Nat)
                })
                .then_some(name.as_str())
        })
        .collect();
    crate::placement::instance_addresses(state, cfg)
        .into_iter()
        .filter(|(id, _)| nat_stacks.contains(id.stack.as_str()))
        .map(|(_, ip)| IpAddr::V4(ip))
        .collect()
}

/// Gateway address of every overlay network that has a subnet. Bound everywhere,
/// since a container can start on this node between two passes.
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

    /// A proxy host routed on the same network as the internal zone must resolve
    /// by its public name inside a container (SPEC's `blog` is on `wan`, so it stays absent).
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

    /// SPEC's route is on `wan`, not the internal network, so it must not leak into this zone.
    #[test]
    fn a_proxy_host_on_a_different_network_is_absent() {
        let state = state_with(SPEC, 1);
        let nodes = vec![("n1".to_string(), Ipv4Addr::new(10, 10, 0, 1))];
        let records = zone_for(&state, &cluster_config(), &nodes, None);

        assert_eq!(answer(&records, "blog"), None);
    }

    #[test]
    fn a_container_on_a_managed_network_gets_answers_it_can_reach() {
        use crate::placement::fixtures::managed_cluster;
        use cl_raft::ids::InstanceId;
        let (cfg, state) = managed_cluster();
        let nodes = vec![("n1".to_string(), Ipv4Addr::new(10, 10, 0, 1))];
        let shared = zone_for(&state, &cfg, &nodes, None);
        let views = zone_views(&state, &cfg, &shared);

        let held = crate::placement::managed_addresses(&state, &cfg);
        let web = InstanceId::new("blog", "web", 0);
        let solo = InstanceId::new("media", "solo", 0);
        let web_managed = held[&web][0].1;
        let solo_managed = held[&solo][0].1;

        // media/solo sits on the managed network only, so web.blog must resolve to the
        // address web holds there, not to its (unreachable) stack overlay address.
        let view = views
            .get(&IpAddr::V4(solo_managed))
            .expect("solo gets its own view");
        assert_eq!(answer(view, "web.blog"), Some(&[web_managed][..]));
        assert_eq!(answer(view, "tasks.web.blog"), Some(&[web_managed][..]));
        // Everything else is the shared zone.
        assert_eq!(
            answer(view, "client.plain"),
            answer(&shared, "client.plain")
        );
        assert_eq!(answer(view, "solo.media"), Some(&[solo_managed][..]));
        let overlay_web = answer(&shared, "web.blog").unwrap().to_vec();
        assert_ne!(overlay_web, vec![web_managed]);

        // web shares its stack overlay with itself and sees the shared answers: no view.
        assert!(!views.contains_key(&IpAddr::V4(web_managed)));
        // A container on no managed network has no view either.
        assert_eq!(views.len(), 1);
    }

    #[test]
    fn no_managed_network_means_no_per_container_views() {
        use crate::placement::fixtures::managed_cluster;
        let (cfg, state) = managed_cluster();
        let shared = zone_for(&state, &cfg, &[], None);
        assert!(zone_views(
            &state,
            &crate::placement::fixtures::cluster_config(),
            &shared
        )
        .is_empty());
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

    /// `nodes_of` follows the live config, so a node added later is picked up.
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

    /// A stack on a nat network keeps the real address for public names;
    /// one without does not.
    #[test]
    fn only_stacks_with_a_nat_network_skip_the_overrides() {
        let cfg = ClusterConfig::parse(
            "\
cluster: { name: test }
networks:
  default: { type: internal, subnet: 10.10.0.0/24, domain: int.example.com }
  pub: { type: public, domain: example.org }
  home: { type: nat }
nodes:
  n1:
    id: 3b0f6c4a-3d3f-4a8e-9b8b-2c1e6a2a9f01
    role: manager
    network: { default: 10.10.0.1, pub: { ipv4: 203.0.113.5 } }
web: { address: cloud, network: default }
",
        )
        .expect("valid cluster config");
        let plain = state_with(PUBLIC_SPEC, 1);
        assert!(nat_clients(&plain, &cfg).is_empty());

        let nat_spec = PUBLIC_SPEC.replace(
            "networks:\n  default: {}",
            "networks:\n  default: {}\n  home: {}",
        );
        let nat = state_with(&nat_spec, 1);
        assert_eq!(nat_clients(&nat, &cfg).len(), 1);
    }

    #[test]
    fn a_network_with_a_subnet_gets_a_resolver_address() {
        let state = state_with(SPEC, 1);
        let gateways = gateways(&state);
        assert_eq!(gateways.len(), 1);
        assert!(gateways[0].to_string().ends_with(".1"));
    }

    #[test]
    fn a_cluster_with_no_overlay_serves_nothing() {
        assert!(gateways(&ClusterState::default()).is_empty());
    }

    /// The dashboard hostname is not a stack route, so `zone_for` must add it explicitly.
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
