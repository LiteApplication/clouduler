//! The client-facing DNS server: answers the cluster's proxied hostnames to
//! real clients, differently depending on which network the client is on.
//!
//! A client inside a private network gets this node's neighbours' addresses on
//! *that* network (a LAN client asking for a public hostname gets the node's
//! LAN address, so it never hairpins through the router and the proxy sees its
//! real address). A hostname on a subnetted network is only resolvable by
//! clients inside that network, mirroring the proxy's own network gate, and
//! everyone else is refused. Names the cluster does not own are forwarded
//! upstream for clients inside a subnetted network and refused for the rest,
//! so a listener on a public address cannot be used as an open resolver.
//!
//! Off by default (`proxy.dns_server.enabled`): port 53 on a node's own
//! addresses is left alone unless asked for. The container-facing resolver on
//! the overlay gateways is `internal_dns` and is unrelated.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use cl_config::{qualify, ClusterConfig, ContainerView, NetworkKind, Node};
use cl_dns::{records_for, DesiredRecord, RecordType};
use cl_net::client_dns::{Classifier, ClientDns, View, ViewAnswer, ViewRecord};
use cl_net::container_net::container_docker_name;
use cl_net::ipam::Allocations;
use cl_raft::node::ClusterNode;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const RELOAD_INTERVAL: Duration = Duration::from_secs(5);

/// How long to wait before retrying a bind that failed, e.g. an address this
/// node does not hold (a public address behind a router) or a port in use.
const BIND_RETRY: Duration = Duration::from_secs(30);

/// Key of the view for clients no network claims. It has no entry, so every
/// query from such a client is refused.
const UNKNOWN_CLIENT: &str = "";

pub fn start(
    cfg: &ClusterConfig,
    cluster: &Arc<ClusterNode>,
    node_name: &str,
) -> Option<JoinHandle<()>> {
    let dns = match ClientDns::new() {
        Ok(dns) => Arc::new(dns),
        Err(e) => {
            warn!(error = %e, "cannot build the client dns server");
            return None;
        }
    };
    let cluster = cluster.clone();
    let boot_cfg = cfg.clone();
    let node_name = node_name.to_string();
    Some(tokio::spawn(async move {
        run(cluster, dns, boot_cfg, node_name).await;
    }))
}

async fn run(
    cluster: Arc<ClusterNode>,
    dns: Arc<ClientDns>,
    boot_cfg: ClusterConfig,
    node_name: String,
) {
    // One serving task per bound address. Reconciled every pass against the
    // live config, so enabling, disabling or retargeting the server needs no
    // restart.
    let mut listeners: HashMap<SocketAddr, JoinHandle<()>> = HashMap::new();
    let mut upstream: Vec<IpAddr> = Vec::new();
    let mut ticker = tokio::time::interval(RELOAD_INTERVAL);

    loop {
        ticker.tick().await;
        let state = cluster.snapshot().await;
        let cfg = crate::daemon::live_config(&state, &boot_cfg);

        let allocations = cl_api::container_router::allocations_of(&state);
        let wanted: BTreeSet<SocketAddr> = if cfg.proxy.dns_server.enabled {
            listen_addrs(&cfg, &allocations, &node_name)
                .into_iter()
                .collect()
        } else {
            BTreeSet::new()
        };
        let upstream_changed = upstream != cfg.proxy.dns_server.upstream;
        if upstream_changed {
            upstream = cfg.proxy.dns_server.upstream.clone();
        }
        listeners.retain(|addr, handle| {
            let keep = wanted.contains(addr) && !handle.is_finished() && !upstream_changed;
            if !keep {
                handle.abort();
            }
            keep
        });
        if wanted.is_empty() {
            continue;
        }

        let hosts = crate::proxy::desired_dns_hosts(&cfg, &state);
        let leader = cluster.quorum().leader().and_then(|id| {
            cfg.nodes
                .values()
                .find(|node| cl_raft::node_id(node.id) == id)
        });
        let no_private = crate::proxy::private_shortcut_denied_hosts(&cfg, &state);
        let views = views_for(&cfg, &allocations, &hosts, &no_private, leader);
        let live = Arc::new(cfg.clone());
        let segments = segments_of(&cfg, &allocations);
        let classify: Classifier = Arc::new(move |ip| {
            // The router itself asks from its segment address, which a wide
            // container subnet may also contain: the segment wins.
            let segment = match ip.to_canonical() {
                std::net::IpAddr::V4(v4) => segments
                    .iter()
                    .find(|(_, subnet)| subnet.contains(&v4))
                    .map(|(name, _)| name.clone()),
                _ => None,
            };
            segment
                .or_else(|| live.client_network(ip).map(str::to_string))
                .unwrap_or_else(|| UNKNOWN_CLIENT.to_string())
        });
        if let Err(e) = dns.reload(&views, classify).await {
            warn!(error = %e, "could not rebuild the client dns views");
            continue;
        }

        for addr in wanted {
            if listeners.contains_key(&addr) {
                continue;
            }
            let dns = dns.clone();
            let upstream = upstream.clone();
            listeners.insert(
                addr,
                tokio::spawn(async move {
                    loop {
                        match dns.serve(addr, &upstream).await {
                            Ok(()) => return,
                            Err(e) => debug!(%addr, error = %e, "client dns not serving here yet"),
                        }
                        tokio::time::sleep(BIND_RETRY).await;
                    }
                }),
            );
            info!(%addr, "binding the client dns server");
        }
    }
}

fn segments_of(cfg: &ClusterConfig, allocations: &Allocations) -> Vec<(String, ipnet::Ipv4Net)> {
    cfg.container_dns_networks()
        .filter_map(|(name, _)| {
            allocations
                .subnet(&container_docker_name(name))
                .map(|subnet| (name.clone(), subnet))
        })
        .collect()
}

fn listen_addrs(
    cfg: &ClusterConfig,
    allocations: &Allocations,
    node_name: &str,
) -> Vec<SocketAddr> {
    let Some(node) = cfg.node(node_name) else {
        return Vec::new();
    };
    let dns = &cfg.proxy.dns_server;
    let networks: Vec<&str> = if dns.networks.is_empty() {
        cfg.networks
            .iter()
            .filter(|(_, net)| net.kind.is_subnetted())
            .map(|(name, _)| name.as_str())
            .collect()
    } else {
        dns.networks
            .iter()
            .map(String::as_str)
            .filter(|name| {
                cfg.networks
                    .get(*name)
                    .is_some_and(|n| n.kind != NetworkKind::Container)
            })
            .collect()
    };
    let mut addrs: Vec<SocketAddr> = Vec::new();
    for network in networks {
        let ips: Vec<IpAddr> = match cfg.networks.get(network).map(|n| n.kind) {
            Some(NetworkKind::Public) => node
                .public_on(network)
                .into_iter()
                .flat_map(|p| {
                    p.ipv4
                        .map(IpAddr::V4)
                        .into_iter()
                        .chain(p.ipv6.map(IpAddr::V6))
                })
                .collect(),
            _ => node.ip_on(network).into_iter().collect(),
        };
        addrs.extend(ips.into_iter().map(|ip| SocketAddr::new(ip, dns.port)));
    }
    // A container network's peers reach the node on its segment address.
    for (network, _) in cfg.container_dns_networks() {
        if let Some(ip) = allocations.node_ip(&container_docker_name(network), node_name) {
            addrs.push(SocketAddr::new(IpAddr::V4(ip), dns.port));
        }
    }
    addrs.sort();
    addrs.dedup();
    addrs
}

/// What one hostname is on: the network its route is on and the nodes that
/// answer for it.
struct Entry {
    network: String,
    nodes: Vec<String>,
    /// False when the route opted out of the per-network address shortcut.
    private_shortcut: bool,
}

impl Entry {
    /// Whether a client on `client_net` may be answered with a node's address
    /// on its own network: the route allows it and so does the client
    /// network's per link `shortcut` setting towards the route's network.
    fn shortcut_for(&self, cfg: &ClusterConfig, client_net: &str) -> bool {
        self.private_shortcut
            && cfg
                .networks
                .get(client_net)
                .is_none_or(|n| n.shortcut.get(&self.network) != Some(&false))
    }
}

/// One view per network a client can be in (every subnetted and public
/// network), from the hostnames the proxy serves (`hosts`, as built by
/// `proxy::desired_dns_hosts`) plus the dashboard on the leader alone.
fn views_for(
    cfg: &ClusterConfig,
    allocations: &Allocations,
    hosts: &BTreeMap<(String, String), BTreeSet<String>>,
    no_private: &BTreeSet<(String, String)>,
    leader: Option<&Node>,
) -> BTreeMap<String, View> {
    let mut by_name: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    for ((host, network), nodes) in hosts {
        let Some(net) = cfg.networks.get(network) else {
            continue;
        };
        by_name
            .entry(qualify(host, net.domain.as_deref()))
            .or_default()
            .push(Entry {
                network: network.clone(),
                nodes: nodes.iter().cloned().collect(),
                private_shortcut: !no_private.contains(&(host.clone(), network.clone())),
            });
    }
    // Writes only land on the leader, so the dashboard names no other node.
    if let (Some(leader), Some(net)) = (leader, cfg.networks.get(&cfg.web.network)) {
        if let Some((name, _)) = cfg.nodes.iter().find(|(_, n)| n.id == leader.id) {
            // On a managed network only a node holding an address there can
            // answer: the leader if it does, otherwise any that does (reads
            // work on every node, a write on a follower is redirected).
            let nodes = if net.is_managed() && cfg.node_address_on(name, &cfg.web.network).is_none()
            {
                cfg.managed_node_addresses(&cfg.web.network)
                    .into_keys()
                    .map(str::to_string)
                    .collect()
            } else {
                vec![name.clone()]
            };
            by_name
                .entry(qualify(&cfg.web.address, net.domain.as_deref()))
                .or_default()
                .push(Entry {
                    network: cfg.web.network.clone(),
                    nodes,
                    private_shortcut: true,
                });
        }
    }

    let mut views = BTreeMap::new();
    for (client_net, net) in &cfg.networks {
        let container = match net.kind {
            NetworkKind::Internal | NetworkKind::Private | NetworkKind::Public => None,
            // Only a network that lets its peers reach the proxy has clients.
            NetworkKind::Container => {
                match cfg.container_dns_networks().find(|(n, _)| *n == client_net) {
                    Some((_, c)) => Some(c),
                    None => continue,
                }
            }
            _ => continue,
        };
        let mut view = View {
            forward: net.kind.is_subnetted(),
            ..View::default()
        };
        for (fqdn, entries) in &by_name {
            // The proxy serves a route on network B to a client on network A
            // exactly when `reaches(A, B)`: B itself, any public B, or a B in
            // A's `access` list (container networks included).
            let reachable: Vec<&Entry> = entries
                .iter()
                .filter(|e| cfg.reaches(client_net, &e.network))
                .collect();
            let answers = answers_for(cfg, allocations, fqdn, client_net, container, &reachable);
            if answers.is_empty() {
                // Either no access, or nobody to send the client to. Both are
                // refusals: forwarding would hand out whatever the public DNS
                // has for a name the cluster owns.
                view.refused.insert(fqdn.clone());
            } else {
                view.records.push(ViewRecord {
                    name: fqdn.clone(),
                    answers,
                });
            }
        }
        views.insert(client_net.clone(), view);
    }
    views
}

/// The answers for a hostname on a managed network: the node address of each
/// node in `entry.nodes` (the nodes holding a healthy container, so a stopped
/// service answers nothing), one A record per owner. When the client's network
/// cannot reach any owner's address and the network is served by the proxy
/// (`proxy: true`), any node's address on it that the client can reach is an
/// entry node that relays to the owner.
fn managed_answers(cfg: &ClusterConfig, client_net: &str, entry: &Entry) -> Vec<ViewAnswer> {
    let reachable = |ip: &std::net::Ipv4Addr| client_can_reach(cfg, client_net, IpAddr::V4(*ip));
    let owners: Vec<std::net::Ipv4Addr> = entry
        .nodes
        .iter()
        .filter_map(|node| cfg.node_address_on(node, &entry.network))
        .filter(reachable)
        .collect();
    if !owners.is_empty() || entry.nodes.is_empty() || !cfg.managed_proxy(&entry.network) {
        return owners.into_iter().map(ViewAnswer::A).collect();
    }
    cfg.managed_node_addresses(&entry.network)
        .into_values()
        .filter(reachable)
        .map(ViewAnswer::A)
        .collect()
}

/// Whether a client on `client_net` can route to `ip`. A network with an
/// `access` list is firewalled from every subnetted network it does not list
/// (`CLDR-NETF`), so an address inside one is unreachable for it; a network
/// without the list is unrestricted. Addresses in no configured subnet (public
/// ones) are always reachable.
fn client_can_reach(cfg: &ClusterConfig, client_net: &str, ip: IpAddr) -> bool {
    if cfg
        .networks
        .get(client_net)
        .is_none_or(|n| n.access.is_none())
    {
        return true;
    }
    !cfg.networks.iter().any(|(name, net)| {
        net.kind.is_subnetted()
            && name != client_net
            && !cfg.reaches(client_net, name)
            && cfg.ip_in_network(name, ip)
    })
}

/// The answers a client on `client_net` gets for `fqdn`: each serving node's
/// address on the client's own network, or failing that on the route's.
fn answers_for(
    cfg: &ClusterConfig,
    allocations: &Allocations,
    fqdn: &str,
    client_net: &str,
    container: Option<ContainerView<'_>>,
    entries: &[&Entry],
) -> Vec<ViewAnswer> {
    let mut records: Vec<DesiredRecord> = Vec::new();
    let mut segment: Vec<ViewAnswer> = Vec::new();
    for entry in entries {
        // A managed network's hostname is answered by the owning node's
        // address there, never a container address.
        if cfg.is_managed_network(&entry.network) {
            for answer in managed_answers(cfg, client_net, entry) {
                if !segment.contains(&answer) {
                    segment.push(answer);
                }
            }
            continue;
        }
        for node_name in &entry.nodes {
            let Some(node) = cfg.nodes.get(node_name) else {
                continue;
            };
            // A container network's client (the router or a peer behind it)
            // is sent to the node's address on a network the router may
            // reach; the proxy gate lets it in by the segment address its
            // connection is DNATed to. Without such an address, the segment
            // address itself.
            if let (Some(_), true) = (container, entry.private_shortcut) {
                let own = cfg.access_of(client_net).iter().find_map(|net| {
                    // A node's address on a managed network is only reachable
                    // through that node, so it is never handed out.
                    if cfg.is_managed_network(net) {
                        return None;
                    }
                    // Only IPv4 node addresses are routed through the segment.
                    let found: Vec<DesiredRecord> = records_for(cfg, fqdn, net, node)
                        .ok()?
                        .into_iter()
                        .filter(|r| r.kind == RecordType::A)
                        .collect();
                    (!found.is_empty()).then_some(found)
                });
                if let Some(found) = own {
                    records.extend(found);
                    continue;
                }
                if let Some(ip) = allocations.node_ip(&container_docker_name(client_net), node_name)
                {
                    segment.push(ViewAnswer::A(ip));
                    continue;
                }
            }
            // A dotted name is used as is, so the network only picks the
            // address.
            let shortcut = entry.shortcut_for(cfg, client_net);
            // A node with no address on the client's subnetted network cannot
            // be reached from it, but every node's proxy (or expose forwarder)
            // relays to the real owner, so any node that has one will do.
            if shortcut
                && client_net != entry.network
                && cfg.networks.get(client_net).is_some_and(|n| n.kind.is_subnetted())
                && !cfg.is_managed_network(client_net)
                && node.ip_on(client_net).is_none()
            {
                let relays: Vec<DesiredRecord> = cfg
                    .nodes
                    .values()
                    .filter(|other| other.ip_on(client_net).is_some())
                    .filter_map(|other| records_for(cfg, fqdn, client_net, other).ok())
                    .flatten()
                    .filter(|r| matches!(r.kind, RecordType::A | RecordType::AAAA))
                    .filter(|r| match r.content.parse::<IpAddr>() {
                        Ok(ip) => client_can_reach(cfg, client_net, ip),
                        Err(_) => false,
                    })
                    .collect();
                if !relays.is_empty() {
                    records.extend(relays);
                    continue;
                }
            }
            let own_net = if shortcut {
                client_net
            } else {
                &entry.network
            };
            let node_records = |net: &str| {
                if cfg.is_managed_network(net) {
                    Err(cl_dns::DnsError::Invalid(format!(
                        "a node's address on the managed network {net} only names its own containers"
                    )))
                } else {
                    records_for(cfg, fqdn, net, node)
                }
            };
            // Only an address the client's own network can actually route to
            // is handed out: the client's network, the route's, then any other
            // network of the node the client's `access` list grants.
            let mut candidates: Vec<&str> = vec![own_net, entry.network.as_str()];
            if cfg
                .networks
                .get(client_net)
                .is_some_and(|n| n.access.is_some())
            {
                candidates.extend(cfg.access_of(client_net).iter().map(String::as_str).filter(
                    |net| {
                        cfg.networks
                            .get(*net)
                            .is_some_and(|n| n.kind.is_subnetted())
                    },
                ));
            }
            for net in candidates {
                let Ok(found) = node_records(net) else {
                    continue;
                };
                let found: Vec<DesiredRecord> = found
                    .into_iter()
                    .filter(|r| match r.content.parse::<IpAddr>() {
                        Ok(ip) => client_can_reach(cfg, client_net, ip),
                        Err(_) => true,
                    })
                    .collect();
                if !found.is_empty() {
                    records.extend(found);
                    break;
                }
            }
        }
    }
    // Whatever path produced an address, the client's network must be able to
    // route to it.
    records.retain(|r| match r.content.parse::<IpAddr>() {
        Ok(ip) => client_can_reach(cfg, client_net, ip),
        Err(_) => true,
    });
    // A cname beside address records is not valid DNS; the publisher collapses
    // them the same way.
    let mut answers: Vec<ViewAnswer> = cl_dns::normalize_desired(&records)
        .into_iter()
        .filter_map(|r| match r.kind {
            RecordType::A => r.content.parse().ok().map(ViewAnswer::A),
            RecordType::AAAA => r.content.parse().ok().map(ViewAnswer::Aaaa),
            RecordType::CNAME => Some(ViewAnswer::Cname(r.content)),
            _ => None,
        })
        .collect();
    answers.extend(segment);
    answers.dedup();
    answers
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn cfg() -> ClusterConfig {
        ClusterConfig::parse(include_str!("../../../examples/cluster.yaml"))
            .expect("example parses")
    }

    fn hosts(rows: &[(&str, &str, &[&str])]) -> BTreeMap<(String, String), BTreeSet<String>> {
        rows.iter()
            .map(|(host, network, nodes)| {
                (
                    (host.to_string(), network.to_string()),
                    nodes.iter().map(|n| n.to_string()).collect(),
                )
            })
            .collect()
    }

    fn alloc() -> Allocations {
        Allocations::new(
            cl_net::ipam::DEFAULT_OVERLAY_POOL.parse().unwrap(),
            cl_net::ipam::DEFAULT_SUBNET_PREFIX,
        )
    }

    fn a(ip: [u8; 4]) -> ViewAnswer {
        ViewAnswer::A(Ipv4Addr::from(ip))
    }

    fn answer<'a>(
        views: &'a BTreeMap<String, View>,
        view: &str,
        name: &str,
    ) -> Option<&'a [ViewAnswer]> {
        views[view]
            .records
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.answers.as_slice())
    }

    /// A LAN client asking for a public-network host gets the node's LAN
    /// address, not the router's public one.
    #[test]
    fn a_lan_client_gets_the_node_address_on_its_own_network() {
        let views = views_for(
            &cfg(),
            &alloc(),
            &hosts(&[("blog", "wan", &["n1"])]),
            &BTreeSet::new(),
            None,
        );
        assert_eq!(
            answer(&views, "lan", "blog.example.com"),
            Some(&[a([192, 168, 1, 1])][..])
        );
        // The tailnet is its own network with its own address for n1.
        assert_eq!(
            answer(&views, "tailscale", "blog.example.com"),
            Some(&[a([10, 10, 0, 1])][..])
        );
    }

    fn managed_cfg() -> ClusterConfig {
        let mut cfg = cfg();
        cfg.networks.insert(
            "mgd".into(),
            serde_yaml::from_str(
                "{ type: private, subnet: 192.168.32.0/24, managed: true, domain: m.example.com }",
            )
            .unwrap(),
        );
        cfg.nodes.get_mut("n1").unwrap().network.insert(
            "mgd".into(),
            cl_config::NodeAddress::Ip("192.168.32.2".parse().unwrap()),
        );
        cfg
    }

    /// A managed route is answered with the owning node's address on the
    /// network, one per owner and never a container address; a service with
    /// no healthy owner (empty node set) has no answer.
    #[test]
    fn a_managed_network_route_is_answered_with_the_owning_node_address() {
        let mut cfg = managed_cfg();
        cfg.nodes.get_mut("n2").unwrap().network.insert(
            "mgd".into(),
            cl_config::NodeAddress::Ip("192.168.32.3".parse().unwrap()),
        );
        let rows = hosts(&[("files", "mgd", &["n1"])]);
        let views = views_for(&cfg, &alloc(), &rows, &BTreeSet::new(), None);
        assert_eq!(
            answer(&views, "mgd", "files.m.example.com"),
            Some(&[a([192, 168, 32, 2])][..])
        );
        // Replicas on both nodes: one address per owner.
        let rows = hosts(&[("files", "mgd", &["n1", "n2"])]);
        let views = views_for(&cfg, &alloc(), &rows, &BTreeSet::new(), None);
        assert_eq!(
            answer(&views, "mgd", "files.m.example.com"),
            Some(&[a([192, 168, 32, 2]), a([192, 168, 32, 3])][..])
        );
        // A client network that does not reach mgd is refused.
        assert!(answer(&views, "wan", "files.m.example.com").is_none());
        assert!(views["wan"].refused.contains("files.m.example.com"));
        // No healthy owner, no record.
        let rows = hosts(&[("files", "mgd", &[])]);
        let views = views_for(&cfg, &alloc(), &rows, &BTreeSet::new(), None);
        assert!(answer(&views, "mgd", "files.m.example.com").is_none());
    }

    /// A node with no address on the managed network (the owner here) gives a
    /// client nothing; with `proxy: true` any addressed node is an entry node.
    #[test]
    fn a_client_that_cannot_reach_the_owner_falls_back_to_an_entry_node() {
        let mut cfg = managed_cfg();
        cfg.nodes.get_mut("n2").unwrap().network.remove("mgd");
        let rows = hosts(&[("files", "mgd", &["n2"])]);
        let views = views_for(&cfg, &alloc(), &rows, &BTreeSet::new(), None);
        assert!(answer(&views, "mgd", "files.m.example.com").is_none());
        cfg.networks.get_mut("mgd").unwrap().proxy = true;
        let views = views_for(&cfg, &alloc(), &rows, &BTreeSet::new(), None);
        assert_eq!(
            answer(&views, "mgd", "files.m.example.com"),
            Some(&[a([192, 168, 32, 2])][..])
        );
        // Nothing runs it: still no record, even with the proxy on.
        let rows = hosts(&[("files", "mgd", &[])]);
        let views = views_for(&cfg, &alloc(), &rows, &BTreeSet::new(), None);
        assert!(answer(&views, "mgd", "files.m.example.com").is_none());
    }

    /// The login portal and the dashboard name only the nodes holding an
    /// address on a managed network.
    #[test]
    fn the_portal_and_dashboard_on_a_managed_network_name_addressed_nodes() {
        let mut cfg = managed_cfg();
        cfg.proxy.auth = Some(
            serde_yaml::from_str(
                "{ host: login, networks: [mgd], issuer: 'https://id.example.com', client_id: c }",
            )
            .unwrap(),
        );
        cfg.web.network = "mgd".into();

        let rows = hosts(&[("login", "mgd", &["n1"])]);
        let views = views_for(&cfg, &alloc(), &rows, &BTreeSet::new(), None);
        assert_eq!(
            answer(&views, "mgd", "login.m.example.com"),
            Some(&[a([192, 168, 32, 2])][..])
        );
        assert!(answer(&views, "wan", "login.m.example.com").is_none());

        // The dashboard goes to the leader when it holds an address there.
        let leader = cfg.nodes["n1"].clone();
        let views = views_for(
            &cfg,
            &alloc(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            Some(&leader),
        );
        assert_eq!(
            answer(&views, "mgd", "cloud.m.example.com"),
            Some(&[a([192, 168, 32, 2])][..])
        );
        // Otherwise to the nodes that do, never to an address-less leader.
        let leader = cfg.nodes["n2"].clone();
        let views = views_for(
            &cfg,
            &alloc(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            Some(&leader),
        );
        assert_eq!(
            answer(&views, "mgd", "cloud.m.example.com"),
            Some(&[a([192, 168, 32, 2])][..])
        );
    }

    /// `private_shortcut: false` hands a LAN client the route's own address.
    #[test]
    fn a_route_can_refuse_the_private_network_shortcut() {
        let no_private = BTreeSet::from([("blog".to_string(), "wan".to_string())]);
        let views = views_for(
            &cfg(),
            &alloc(),
            &hosts(&[("blog", "wan", &["n1"])]),
            &no_private,
            None,
        );
        let lan = answer(&views, "lan", "blog.example.com").expect("answered");
        assert_ne!(lan, &[a([192, 168, 1, 1])][..]);
        assert_eq!(lan, answer(&views, "wan", "blog.example.com").unwrap());
    }

    /// A client behind an `access` list never gets an address in a subnet it
    /// is firewalled from: it falls back to the node's address on a network
    /// it may reach.
    #[test]
    fn answers_skip_addresses_the_client_cannot_route_to() {
        let mut cfg = cfg();
        cfg.networks.insert(
            "tmp".into(),
            serde_yaml::from_str(
                "{ type: private, subnet: 192.168.27.0/24, access: [lan], domain: tmp.example.com }",
            )
            .unwrap(),
        );
        // The route's public address sits inside the (denied) tailscale subnet.
        cfg.nodes.get_mut("n1").unwrap().network.insert(
            "wan".into(),
            serde_yaml::from_str("{ ipv4: 10.10.0.1 }").unwrap(),
        );
        let rows = hosts(&[("blog", "wan", &["n1"])]);
        let views = views_for(&cfg, &alloc(), &rows, &BTreeSet::new(), None);
        assert_eq!(
            answer(&views, "tmp", "blog.example.com"),
            Some(&[a([192, 168, 1, 1])][..])
        );
        // A client with no access list is unrestricted and keeps the old answer.
        assert_eq!(
            answer(&views, "lan", "blog.example.com"),
            Some(&[a([192, 168, 1, 1])][..])
        );
        assert_eq!(
            answer(&views, "wan", "blog.example.com"),
            Some(&[a([10, 10, 0, 1])][..])
        );

        // Nothing reachable at all: refused rather than an unroutable answer.
        cfg.networks.get_mut("tmp").unwrap().access = Some(vec![]);
        let views = views_for(&cfg, &alloc(), &rows, &BTreeSet::new(), None);
        assert!(views["tmp"].refused.contains("blog.example.com"));
    }

    /// With no LAN address, a LAN client falls back to the node's public one.
    #[test]
    fn a_node_without_an_address_on_the_clients_network_falls_back_to_the_route() {
        let mut cfg = cfg();
        cfg.nodes.get_mut("n2").unwrap().network.remove("lan");
        let views = views_for(
            &cfg,
            &alloc(),
            &hosts(&[("blog", "wan", &["n2"])]),
            &BTreeSet::new(),
            None,
        );
        // Every node's proxy relays to the owner, so a node that has a LAN
        // address answers for it (n1 here).
        assert_eq!(
            answer(&views, "lan", "blog.example.com"),
            Some(&[a([192, 168, 1, 1])][..])
        );
        // No node has one: the route's own address, as before.
        cfg.nodes.get_mut("n1").unwrap().network.remove("lan");
        let views = views_for(
            &cfg,
            &alloc(),
            &hosts(&[("blog", "wan", &["n2"])]),
            &BTreeSet::new(),
            None,
        );
        assert_eq!(
            answer(&views, "lan", "blog.example.com"),
            Some(&[ViewAnswer::Cname("n2.example.net".into())][..])
        );
    }

    /// `shortcut: { wan: false }` on the LAN gives LAN clients the route's own
    /// address, while the tailnet (no key) keeps the shortcut.
    #[test]
    fn a_per_link_shortcut_setting_only_affects_that_client_network() {
        let mut cfg = cfg();
        cfg.networks
            .get_mut("lan")
            .unwrap()
            .shortcut
            .insert("wan".into(), false);
        let views = views_for(
            &cfg,
            &alloc(),
            &hosts(&[("blog", "wan", &["n1"])]),
            &BTreeSet::new(),
            None,
        );
        assert_eq!(
            answer(&views, "lan", "blog.example.com"),
            answer(&views, "wan", "blog.example.com")
        );
        assert_eq!(
            answer(&views, "tailscale", "blog.example.com"),
            Some(&[a([10, 10, 0, 1])][..])
        );
        // An explicit true is the default.
        cfg.networks
            .get_mut("lan")
            .unwrap()
            .shortcut
            .insert("wan".into(), true);
        let views = views_for(
            &cfg,
            &alloc(),
            &hosts(&[("blog", "wan", &["n1"])]),
            &BTreeSet::new(),
            None,
        );
        assert_eq!(
            answer(&views, "lan", "blog.example.com"),
            Some(&[a([192, 168, 1, 1])][..])
        );
    }

    #[test]
    fn a_public_client_gets_the_public_addresses() {
        let views = views_for(
            &cfg(),
            &alloc(),
            &hosts(&[("blog", "wan", &["n1"])]),
            &BTreeSet::new(),
            None,
        );
        assert_eq!(
            answer(&views, "wan", "blog.example.com"),
            Some(
                &[
                    a([203, 0, 113, 5]),
                    ViewAnswer::Aaaa("2001:db8::5".parse().unwrap())
                ][..]
            )
        );
        assert!(!views["wan"].forward && views["lan"].forward);
    }

    /// A route on the LAN is resolvable from the LAN only: the proxy would
    /// answer 403 to anyone else, so DNS does not hand out the way in.
    #[test]
    fn a_host_on_a_private_network_is_refused_to_everyone_else() {
        let views = views_for(
            &cfg(),
            &alloc(),
            &hosts(&[("printer", "lan", &["n1"])]),
            &BTreeSet::new(),
            None,
        );
        assert_eq!(
            answer(&views, "lan", "printer.lan.example.com"),
            Some(&[a([192, 168, 1, 1])][..])
        );
        for other in ["wan", "tailscale"] {
            assert!(answer(&views, other, "printer.lan.example.com").is_none());
            assert!(views[other].refused.contains("printer.lan.example.com"));
        }
    }

    #[test]
    fn only_the_listed_nodes_are_named() {
        let views = views_for(
            &cfg(),
            &alloc(),
            &hosts(&[("svc", "tailscale", &["n1", "n3"])]),
            &BTreeSet::new(),
            None,
        );
        assert_eq!(
            answer(&views, "tailscale", "svc.int.example.com"),
            Some(&[a([10, 10, 0, 1]), a([10, 10, 0, 3])][..])
        );
    }

    #[test]
    fn the_dashboard_names_the_leader_alone() {
        let cfg = cfg();
        let leader = cfg.node("n3");
        let views = views_for(&cfg, &alloc(), &BTreeMap::new(), &BTreeSet::new(), leader);
        assert_eq!(
            answer(&views, "tailscale", "cloud.int.example.com"),
            Some(&[a([10, 10, 0, 3])][..])
        );
        // No leader, no answer, and no stale guess: the name just isn't ours.
        let none = views_for(&cfg, &alloc(), &BTreeMap::new(), &BTreeSet::new(), None);
        assert!(answer(&none, "tailscale", "cloud.int.example.com").is_none());
    }

    #[test]
    fn egress_networks_have_no_view() {
        let views = views_for(&cfg(), &alloc(), &BTreeMap::new(), &BTreeSet::new(), None);
        assert!(!views.contains_key("vpn"));
        assert_eq!(views.len(), 3);
    }

    #[test]
    fn the_server_listens_on_this_nodes_subnetted_addresses_by_default() {
        let mut cfg = cfg();
        cfg.proxy.dns_server.enabled = true;
        let addrs = listen_addrs(&cfg, &alloc(), "n1");
        assert_eq!(
            addrs,
            vec![
                "10.10.0.1:53".parse().unwrap(),
                "192.168.1.1:53".parse().unwrap()
            ]
        );
        // n3 is only on the tailnet.
        assert_eq!(
            listen_addrs(&cfg, &alloc(), "n3"),
            vec!["10.10.0.3:53".parse().unwrap()]
        );

        cfg.proxy.dns_server.networks = vec!["wan".into()];
        cfg.proxy.dns_server.port = 5353;
        assert_eq!(
            listen_addrs(&cfg, &alloc(), "n1"),
            vec![
                "203.0.113.5:5353".parse().unwrap(),
                "[2001:db8::5]:5353".parse().unwrap()
            ]
        );
    }

    /// The example cluster's `tailnet` container network, with the DNS server
    /// on and every node given an address on its segment.
    fn tailnet() -> (ClusterConfig, Allocations) {
        let mut cfg = cfg();
        cfg.proxy.dns_server.enabled = true;
        let mut alloc = alloc();
        let segment = container_docker_name("tailnet");
        alloc.allocate_network(&segment, &[]).expect("segment");
        for node in ["n1", "n2"] {
            alloc
                .allocate_node_ip(&segment, node)
                .expect("node address");
        }
        (cfg, alloc)
    }

    /// A tailnet peer is sent to the node's address on a network the router
    /// may reach (the shortcut), for public routes and for routes on a
    /// network the container network may reach; not for the rest.
    #[test]
    fn a_container_network_peer_gets_the_node_address_on_an_allowed_network() {
        let (cfg, alloc) = tailnet();
        let segment = container_docker_name("tailnet");
        let n1 = alloc.node_ip(&segment, "n1").unwrap();
        let views = views_for(
            &cfg,
            &alloc,
            &hosts(&[
                ("blog", "wan", &["n1"]),
                ("nas", "lan", &["n1"]),
                ("vpn", "tailscale", &["n1"]),
            ]),
            &BTreeSet::new(),
            None,
        );
        assert_eq!(
            answer(&views, "tailnet", "blog.example.com"),
            Some(&[a([192, 168, 1, 1])][..])
        );
        // `lan` is in allowed_networks, `tailscale` is not.
        assert_eq!(
            answer(&views, "tailnet", "nas.lan.example.com"),
            Some(&[a([192, 168, 1, 1])][..])
        );
        assert!(views["tailnet"].refused.contains("vpn.int.example.com"));
        assert!(!views["tailnet"].forward);
        // They reach the server on that address.
        assert!(listen_addrs(&cfg, &alloc, "n1").contains(&SocketAddr::new(n1.into(), 53)));
    }

    #[test]
    fn a_container_network_peer_can_be_denied_the_shortcut() {
        let (cfg, alloc) = tailnet();
        let no_private = BTreeSet::from([("blog".to_string(), "wan".to_string())]);
        let views = views_for(
            &cfg,
            &alloc,
            &hosts(&[("blog", "wan", &["n1"])]),
            &no_private,
            None,
        );
        assert_eq!(
            answer(&views, "tailnet", "blog.example.com"),
            Some(
                &[
                    a([203, 0, 113, 5]),
                    ViewAnswer::Aaaa("2001:db8::5".parse().unwrap())
                ][..]
            )
        );
    }

    #[test]
    fn a_container_network_without_proxy_access_has_no_view() {
        let (mut cfg, alloc) = tailnet();
        cfg.networks
            .get_mut("tailnet")
            .unwrap()
            .container
            .as_mut()
            .unwrap()
            .proxy = false;
        let views = views_for(&cfg, &alloc, &hosts(&[]), &BTreeSet::new(), None);
        assert!(!views.contains_key("tailnet"));
        assert_eq!(cfg.client_network("100.64.1.1".parse().unwrap()), None);
    }

    #[test]
    fn a_tailnet_address_is_classified_as_that_container_network() {
        let (cfg, _) = tailnet();
        assert_eq!(
            cfg.client_network("100.64.1.1".parse().unwrap()),
            Some("tailnet")
        );
    }

    /// Only IPv4 addresses are routed through the segment, so the shortcut
    /// never answers with an AAAA, even from a dual-stack public network.
    #[test]
    fn the_container_shortcut_answers_ipv4_only() {
        let (mut cfg, alloc) = tailnet();
        cfg.networks.get_mut("tailnet").unwrap().access = Some(vec!["wan".to_string()]);
        let views = views_for(
            &cfg,
            &alloc,
            &hosts(&[("blog", "wan", &["n1"])]),
            &BTreeSet::new(),
            None,
        );
        assert_eq!(
            answer(&views, "tailnet", "blog.example.com"),
            Some(&[a([203, 0, 113, 5])][..])
        );
    }

    /// A subnetted client network with `access` to another subnetted one is
    /// served its routes, answered with the node's address on its own
    /// network; without access the name is refused; opting out of the
    /// shortcut answers with the route network's address.
    #[test]
    fn a_subnetted_network_with_access_gets_the_shortcut_answer() {
        let mut cfg = cfg();
        let rows = hosts(&[("nas", "lan", &["n1"])]);
        let denied = views_for(&cfg, &alloc(), &rows, &BTreeSet::new(), None);
        assert!(denied["tailscale"].refused.contains("nas.lan.example.com"));
        assert!(answer(&denied, "tailscale", "nas.lan.example.com").is_none());

        cfg.networks.get_mut("tailscale").unwrap().access = Some(vec!["lan".to_string()]);
        let views = views_for(&cfg, &alloc(), &rows, &BTreeSet::new(), None);
        assert_eq!(
            answer(&views, "tailscale", "nas.lan.example.com"),
            Some(&[a([10, 10, 0, 1])][..])
        );

        let no_private = BTreeSet::from([("nas".to_string(), "lan".to_string())]);
        let views = views_for(&cfg, &alloc(), &rows, &no_private, None);
        assert_eq!(
            answer(&views, "tailscale", "nas.lan.example.com"),
            Some(&[a([192, 168, 1, 1])][..])
        );
    }
}
