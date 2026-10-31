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

use cl_config::{qualify, ClusterConfig, NetworkKind, Node};
use cl_dns::{records_for, DesiredRecord, RecordType};
use cl_net::client_dns::{Classifier, ClientDns, View, ViewAnswer, ViewRecord};
use cl_raft::node::ClusterNode;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// How often the views are rebuilt and the listeners reconciled.
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
    let mut ticker = tokio::time::interval(RELOAD_INTERVAL);

    loop {
        ticker.tick().await;
        let state = cluster.state().await;
        let cfg = crate::daemon::live_config(&state, &boot_cfg);

        let wanted: BTreeSet<SocketAddr> = if cfg.proxy.dns_server.enabled {
            listen_addrs(&cfg, &node_name).into_iter().collect()
        } else {
            BTreeSet::new()
        };
        listeners.retain(|addr, handle| {
            let keep = wanted.contains(addr) && !handle.is_finished();
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
        let views = views_for(&cfg, &hosts, leader);
        let live = Arc::new(cfg.clone());
        let classify: Classifier = Arc::new(move |ip| {
            live.client_network(ip)
                .unwrap_or(UNKNOWN_CLIENT)
                .to_string()
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
            listeners.insert(
                addr,
                tokio::spawn(async move {
                    loop {
                        match dns.serve(addr).await {
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

/// This node's address on every network the server listens on.
fn listen_addrs(cfg: &ClusterConfig, node_name: &str) -> Vec<SocketAddr> {
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
        dns.networks.iter().map(String::as_str).collect()
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
    addrs.sort();
    addrs.dedup();
    addrs
}

/// What one hostname is on: the network its route is on and the nodes that
/// answer for it.
struct Entry {
    network: String,
    nodes: Vec<String>,
}

/// One view per network a client can be in (every subnetted and public
/// network), from the hostnames the proxy serves (`hosts`, as built by
/// `proxy::desired_dns_hosts`) plus the dashboard on the leader alone.
fn views_for(
    cfg: &ClusterConfig,
    hosts: &BTreeMap<(String, String), BTreeSet<String>>,
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
            });
    }
    // Writes only land on the leader, so the dashboard names no other node.
    if let (Some(leader), Some(net)) = (leader, cfg.networks.get(&cfg.web.network)) {
        if let Some((name, _)) = cfg.nodes.iter().find(|(_, n)| n.id == leader.id) {
            by_name
                .entry(qualify(&cfg.web.address, net.domain.as_deref()))
                .or_default()
                .push(Entry {
                    network: cfg.web.network.clone(),
                    nodes: vec![name.clone()],
                });
        }
    }

    let mut views = BTreeMap::new();
    for (client_net, net) in &cfg.networks {
        if !matches!(
            net.kind,
            NetworkKind::Internal | NetworkKind::Private | NetworkKind::Public
        ) {
            continue;
        }
        let mut view = View {
            forward: net.kind.is_subnetted(),
            ..View::default()
        };
        for (fqdn, entries) in &by_name {
            // The proxy serves a route on a subnetted network only to peers
            // inside its subnet; a public route to anyone.
            let reachable: Vec<&Entry> = entries
                .iter()
                .filter(|e| {
                    cfg.networks[&e.network].kind == NetworkKind::Public || e.network == *client_net
                })
                .collect();
            let answers = answers_for(cfg, fqdn, client_net, &reachable);
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

/// The answers a client on `client_net` gets for `fqdn`: each serving node's
/// address on the client's own network, or failing that on the route's.
fn answers_for(
    cfg: &ClusterConfig,
    fqdn: &str,
    client_net: &str,
    entries: &[&Entry],
) -> Vec<ViewAnswer> {
    let mut records: Vec<DesiredRecord> = Vec::new();
    for entry in entries {
        for node_name in &entry.nodes {
            let Some(node) = cfg.nodes.get(node_name) else {
                continue;
            };
            // A dotted name is used as is, so the network only picks the
            // address.
            let found = records_for(cfg, fqdn, client_net, node)
                .or_else(|_| records_for(cfg, fqdn, &entry.network, node));
            if let Ok(found) = found {
                records.extend(found);
            }
        }
    }
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
        let views = views_for(&cfg(), &hosts(&[("blog", "wan", &["n1"])]), None);
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

    /// With no LAN address, a LAN client falls back to the node's public one.
    #[test]
    fn a_node_without_an_address_on_the_clients_network_falls_back_to_the_route() {
        let mut cfg = cfg();
        cfg.nodes.get_mut("n2").unwrap().network.remove("lan");
        let views = views_for(&cfg, &hosts(&[("blog", "wan", &["n2"])]), None);
        assert_eq!(
            answer(&views, "lan", "blog.example.com"),
            Some(&[ViewAnswer::Cname("n2.example.net".into())][..])
        );
    }

    #[test]
    fn a_public_client_gets_the_public_addresses() {
        let views = views_for(&cfg(), &hosts(&[("blog", "wan", &["n1"])]), None);
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
        let views = views_for(&cfg(), &hosts(&[("printer", "lan", &["n1"])]), None);
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
        let views = views_for(&cfg(), &hosts(&[("svc", "tailscale", &["n1", "n3"])]), None);
        assert_eq!(
            answer(&views, "tailscale", "svc.int.example.com"),
            Some(&[a([10, 10, 0, 1]), a([10, 10, 0, 3])][..])
        );
    }

    #[test]
    fn the_dashboard_names_the_leader_alone() {
        let cfg = cfg();
        let leader = cfg.node("n3");
        let views = views_for(&cfg, &BTreeMap::new(), leader);
        assert_eq!(
            answer(&views, "tailscale", "cloud.int.example.com"),
            Some(&[a([10, 10, 0, 3])][..])
        );
        // No leader, no answer, and no stale guess: the name just isn't ours.
        let none = views_for(&cfg, &BTreeMap::new(), None);
        assert!(answer(&none, "tailscale", "cloud.int.example.com").is_none());
    }

    #[test]
    fn egress_networks_have_no_view() {
        let views = views_for(&cfg(), &BTreeMap::new(), None);
        assert!(!views.contains_key("vpn"));
        assert_eq!(views.len(), 3);
    }

    #[test]
    fn the_server_listens_on_this_nodes_subnetted_addresses_by_default() {
        let mut cfg = cfg();
        cfg.proxy.dns_server.enabled = true;
        let addrs = listen_addrs(&cfg, "n1");
        assert_eq!(
            addrs,
            vec![
                "10.10.0.1:53".parse().unwrap(),
                "192.168.1.1:53".parse().unwrap()
            ]
        );
        // n3 is only on the tailnet.
        assert_eq!(
            listen_addrs(&cfg, "n3"),
            vec!["10.10.0.3:53".parse().unwrap()]
        );

        cfg.proxy.dns_server.networks = vec!["wan".into()];
        cfg.proxy.dns_server.port = 5353;
        assert_eq!(
            listen_addrs(&cfg, "n1"),
            vec![
                "203.0.113.5:5353".parse().unwrap(),
                "[2001:db8::5]:5353".parse().unwrap()
            ]
        );
    }
}
