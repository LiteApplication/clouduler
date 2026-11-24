//! Host routing and firewall of managed private networks (see
//! `cl_net::managed`): per-container routes to the nodes hosting them, the
//! forward filter and the source NAT, re-derived from the live config and
//! replicated placement every few seconds so a moved container, a node that
//! left or an edited `access` list takes effect without a restart. A node that
//! implements no managed network carries none of it.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cl_config::{ClusterConfig, NetworkKind};
use cl_net::iptables::{apply_plan, list_chains, remove_chain};
use cl_net::managed::{
    apply_routes, managed_plan, Hop, ManagedInput, ManagedNetRules, FORWARD_CHAIN, NAT_CHAIN,
};
use cl_net::managed_tunnel::{ensure_tunnels, hops, inner_addr, tunnel_name, TunnelSpec};
use cl_net::vxlan::bridge_name;
use cl_net::{managed_key, Allocations};
use cl_raft::node::ClusterNode;
use cl_raft::state::ClusterState;
use ipnet::{IpNet, Ipv4Net};
use tracing::{info, warn};

const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// Subnets of the cluster networks whose traffic may enter managed network
/// `target`: a network without an `access` list is unrestricted (as for
/// `CLDR-NETF`), one with a list must name `target`. A `container` network
/// counts through its routed range.
fn allowed_sources(cfg: &ClusterConfig, target: &str) -> Vec<Ipv4Net> {
    cfg.networks
        .iter()
        .filter(|(name, net)| {
            name.as_str() != target
                && matches!(
                    net.kind,
                    NetworkKind::Internal | NetworkKind::Private | NetworkKind::Container
                )
                && (net.access.is_none() || cfg.reaches(name, target))
        })
        .filter_map(|(_, net)| match (net.kind, net.subnet) {
            (NetworkKind::Container, _) => net.routed_subnet(),
            (_, Some(IpNet::V4(subnet))) => Some(subnet),
            _ => None,
        })
        .collect()
}

/// The internal and inner address (see `cl_net::managed_tunnel::inner_addr`) of
/// every other node implementing a managed network, when this node implements
/// one too. Two nodes whose inner addresses would collide get no tunnel, with a
/// warning.
pub fn peers(cfg: &ClusterConfig, node: &str) -> Vec<(Ipv4Addr, Ipv4Addr)> {
    if crate::overlay::managed_local(cfg, node).is_empty() {
        return Vec::new();
    }
    let managed = cfg.managed_networks();
    let all: Vec<(&String, Ipv4Addr)> = cfg
        .nodes
        .iter()
        .filter(|(_, n)| {
            managed
                .iter()
                .any(|(name, _, _)| n.network.contains_key(*name))
        })
        .filter_map(|(name, n)| match cfg.internal_ip(n) {
            Some(IpAddr::V4(ip)) => Some((name, ip)),
            _ => None,
        })
        .collect();
    all.iter()
        .filter(|(name, _)| name.as_str() != node)
        .filter_map(|(name, internal)| {
            let inner = inner_addr(*internal)?;
            let clash = all.iter().any(|(other, ip)| other != name && inner_addr(*ip) == Some(inner));
            if clash {
                warn!(node = %name, "the managed network tunnel address of this node collides with another's, no tunnel to it");
                return None;
            }
            Some((*internal, inner))
        })
        .collect()
}

/// What this node must enforce for the managed networks it implements.
pub fn input_for(cfg: &ClusterConfig, node: &str) -> ManagedInput {
    let local = crate::overlay::managed_local(cfg, node);
    let nets = local
        .iter()
        .map(|m| ManagedNetRules {
            name: m.name.clone(),
            subnet: m.subnet,
            bridge: bridge_name(&managed_key(&m.name)),
            gateway: cl_config::managed_gateway(m.subnet),
            sources: allowed_sources(cfg, &m.name),
        })
        .collect::<Vec<_>>();
    let internal = |n: &cl_config::Node| match cfg.internal_ip(n) {
        Some(IpAddr::V4(ip)) => Some(ip),
        _ => None,
    };
    let own = cfg.node(node).and_then(internal);
    let peers = peers(cfg, node);
    let mut node_addrs: Vec<Ipv4Addr> = cfg
        .nodes
        .values()
        .filter(|n| {
            cfg.managed_networks()
                .iter()
                .any(|(name, _, _)| n.network.contains_key(*name))
        })
        .filter_map(internal)
        .collect();
    // Another node's connections arrive from its inner address when they come
    // through a tunnel.
    node_addrs.extend(peers.iter().map(|(_, inner)| *inner));
    let own_inner = own.and_then(inner_addr).filter(|_| !nets.is_empty());
    let tunnel_devs = own
        .map(|own| {
            peers
                .iter()
                .map(|(remote, _)| tunnel_name(own, *remote))
                .collect()
        })
        .unwrap_or_default();
    ManagedInput {
        nets,
        node_addrs,
        own_addr: own,
        own_inner,
        tunnel_devs,
    }
}

/// Every container of a managed network this node implements that another
/// node runs, with the internal address of that node.
pub fn wanted_routes(
    state: &ClusterState,
    cfg: &ClusterConfig,
    node: &str,
) -> BTreeMap<Ipv4Addr, Ipv4Addr> {
    let mut out = BTreeMap::new();
    let local = crate::overlay::managed_local(cfg, node);
    if local.is_empty() {
        return out;
    }
    let Some(allocations) = Allocations::decode(state.overlay.as_deref()) else {
        return out;
    };
    for net in &local {
        for (instance, ip) in allocations.instances(&managed_key(&net.name)) {
            let Some(assignment) = state.assignments.get(instance) else {
                continue;
            };
            if assignment.node == node {
                continue;
            }
            let Some(IpAddr::V4(via)) = cfg.node(&assignment.node).and_then(|n| cfg.internal_ip(n))
            else {
                continue;
            };
            out.insert(ip, via);
        }
    }
    out
}

fn remove_chains() -> Result<()> {
    for (table, chain) in [("filter", FORWARD_CHAIN), ("nat", NAT_CHAIN)] {
        if list_chains(table)?.iter().any(|c| c == chain) {
            // The chain goes together with its jump (see `remove_chain`).
            remove_chain(table, chain)?;
            info!(chain, "removed the managed network chain");
        }
    }
    Ok(())
}

/// The tunnels this node needs: one to every peer.
fn tunnel_specs(cfg: &ClusterConfig, node: &str) -> Vec<TunnelSpec> {
    let Some(own) = cfg.node(node).and_then(|n| match cfg.internal_ip(n) {
        Some(IpAddr::V4(ip)) => Some(ip),
        _ => None,
    }) else {
        return Vec::new();
    };
    peers(cfg, node)
        .into_iter()
        .map(|(remote, _)| TunnelSpec::new(own, remote))
        .collect()
}

fn apply(
    input: &ManagedInput,
    routes: &BTreeMap<Ipv4Addr, Ipv4Addr>,
    peers: &[(Ipv4Addr, Ipv4Addr)],
    specs: &[TunnelSpec],
) -> Result<()> {
    let plan = managed_plan(input);
    // Tunnels first: the routes go through them. A peer whose tunnel cannot be
    // built keeps the plain route via its internal address, which only a
    // network that carries any destination (not a WireGuard mesh) delivers.
    let (devs, errors) = ensure_tunnels(specs);
    {
        static LAST: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let mut last = LAST.lock().unwrap_or_else(|p| p.into_inner());
        if *last != errors {
            if !errors.is_empty() {
                warn!(errors = ?errors, "managed network tunnels not (all) built, falling back to plain routes via the internal address");
            }
            *last = errors;
        }
    }
    let tunnels: BTreeMap<Ipv4Addr, String> = specs
        .iter()
        .filter(|s| devs.contains(&s.dev))
        .map(|s| (s.remote, s.dev.clone()))
        .collect();
    let routes: BTreeMap<Ipv4Addr, Hop> = hops(routes, peers, &tunnels);
    let routes = &routes;
    // The routes do not depend on the firewall: a failing chain must not leave
    // a remote container unreachable (its address would be ARPed on the local
    // bridge, where nobody answers).
    let firewall = (|| -> Result<()> {
        if plan.chains.is_empty() {
            remove_chains()?;
        } else {
            // Forwarding is what lets a node carry a client to a container.
            std::fs::write("/proc/sys/net/ipv4/ip_forward", b"1")
                .context("enabling ipv4 forwarding")?;
            apply_plan(&plan)?;
        }
        Ok(())
    })();
    let routed = apply_routes(routes);
    firewall?;
    routed?;
    Ok(())
}

pub fn start(
    cfg: &ClusterConfig,
    node_name: &str,
    cluster: &Arc<ClusterNode>,
) -> tokio::task::JoinHandle<()> {
    let cluster = cluster.clone();
    let node_name = node_name.to_string();
    let boot_cfg = cfg.clone();

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);
        // Nothing to clean up once an empty pass has run; skips the iptables
        // and `ip` calls on a cluster that never uses managed networks.
        let mut clean = false;
        loop {
            ticker.tick().await;
            let state = cluster.snapshot().await;
            let cfg = crate::daemon::live_config(&state, &boot_cfg);
            let input = input_for(&cfg, &node_name);
            if input.nets.is_empty() && clean {
                continue;
            }
            let routes = wanted_routes(&state, &cfg, &node_name);
            let peers = peers(&cfg, &node_name);
            let specs = tunnel_specs(&cfg, &node_name);
            let idle = input.nets.is_empty();
            let result =
                tokio::task::spawn_blocking(move || apply(&input, &routes, &peers, &specs))
                    .await
                    .context("managed network task panicked")
                    .and_then(|r| r);
            match result {
                Ok(()) => clean = idle,
                Err(e) => {
                    warn!(error = %e, "cannot apply the managed network routing and firewall")
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ClusterConfig {
        let mut cfg = ClusterConfig::parse(include_str!("../../../examples/cluster.yaml")).unwrap();
        cfg.networks.insert(
            "mgd".into(),
            serde_yaml::from_str("{ type: private, subnet: 192.168.32.0/24, managed: true }")
                .unwrap(),
        );
        for (i, node) in cfg.nodes.values_mut().take(2).enumerate() {
            node.network.insert(
                "mgd".into(),
                cl_config::NodeAddress::Ip(format!("192.168.32.{}", i + 2).parse().unwrap()),
            );
        }
        cfg
    }

    #[test]
    fn only_nodes_implementing_a_managed_network_get_rules() {
        let cfg = cfg();
        let names: Vec<&String> = cfg.nodes.keys().collect();
        let on = input_for(&cfg, names[0]);
        assert_eq!(on.nets.len(), 1);
        assert!(on.own_addr.is_some());
        // Both nodes' internal addresses and the other's tunnel address.
        assert_eq!(on.node_addrs.len(), 3);
        assert!(on.own_inner.is_some());
        assert_eq!(on.tunnel_devs.len(), 1);
        let off = input_for(&cfg, names[2]);
        assert!(off.nets.is_empty());
    }

    #[test]
    fn an_access_list_that_omits_the_network_is_not_an_allowed_source() {
        let mut cfg = cfg();
        let sources = |cfg: &ClusterConfig| allowed_sources(cfg, "mgd");
        let before = sources(&cfg);
        assert!(!before.is_empty());
        for net in cfg.networks.values_mut() {
            if net.kind != NetworkKind::Public && !net.managed {
                net.access = Some(vec![]);
            }
        }
        assert!(sources(&cfg).is_empty());
        for net in cfg.networks.values_mut() {
            if net.kind != NetworkKind::Public && !net.managed {
                net.access = Some(vec!["mgd".into()]);
            }
        }
        // The example's container network lists only `lan`, so it joins now.
        assert_eq!(sources(&cfg).len(), before.len() + 1);
    }

    #[test]
    fn a_container_network_is_a_source_through_its_routed_range() {
        let mut cfg = cfg();
        let routed: Ipv4Net = "100.64.0.0/10".parse().unwrap();
        // The example's tailnet lists only `lan`: no grant yet.
        assert!(cfg.networks["tailnet"].kind == NetworkKind::Container);
        assert!(!allowed_sources(&cfg, "mgd").contains(&routed));
        cfg.networks.get_mut("tailnet").unwrap().access = Some(vec!["lan".into(), "mgd".into()]);
        assert!(allowed_sources(&cfg, "mgd").contains(&routed));
        // No access list at all counts as unrestricted, like any other source.
        cfg.networks.get_mut("tailnet").unwrap().access = None;
        assert!(allowed_sources(&cfg, "mgd").contains(&routed));
        let names: Vec<String> = cfg.nodes.keys().cloned().collect();
        let (filter, _) = accepted(&cfg, &names[0]);
        assert!(accepts(&filter, "100.64.0.0/10"), "{filter:?}");
    }

    /// The CLDR-MNG rules a node renders for `mgd`, accept rules by source.
    fn accepted(cfg: &ClusterConfig, node: &str) -> (Vec<String>, Vec<String>) {
        let plan = cl_net::managed::managed_plan(&input_for(cfg, node));
        let filter = plan
            .chain(cl_net::managed::FORWARD_CHAIN)
            .map(|c| c.rules.clone())
            .unwrap_or_default();
        let nat = plan
            .chain(cl_net::managed::NAT_CHAIN)
            .map(|c| c.rules.clone())
            .unwrap_or_default();
        (filter, nat)
    }

    fn accepts(rules: &[String], source: &str) -> bool {
        rules
            .iter()
            .any(|r| r.contains(&format!("-s {source} ")) && r.ends_with("ACCEPT"))
    }

    #[test]
    fn the_plan_accepts_networks_without_an_access_list_and_those_that_list_the_network() {
        let mut cfg = cfg();
        let names: Vec<String> = cfg.nodes.keys().cloned().collect();
        let lan_subnet = |cfg: &ClusterConfig, n: &str| cfg.networks[n].subnet.unwrap().to_string();
        let mut private = cfg
            .networks
            .iter()
            .filter(|(_, n)| n.kind == NetworkKind::Private && !n.managed)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        private.sort();
        let lan = private.first().cloned().expect("a private network");
        let lan_src = lan_subnet(&cfg, &lan);

        // No access list at all: unrestricted, so accepted.
        cfg.networks.get_mut(&lan).unwrap().access = None;
        let (filter, nat) = accepted(&cfg, &names[0]);
        assert!(accepts(&filter, &lan_src), "{filter:?}");
        assert!(filter.last().unwrap().ends_with("DROP"));
        assert_eq!(nat.len(), 3);

        // An access list that omits the network: denied, the default DROP remains.
        cfg.networks.get_mut(&lan).unwrap().access = Some(vec![]);
        let (filter, _) = accepted(&cfg, &names[0]);
        assert!(!accepts(&filter, &lan_src));
        assert!(filter.last().unwrap().ends_with("DROP"));

        // One that names it: allowed again.
        cfg.networks.get_mut(&lan).unwrap().access = Some(vec!["mgd".into()]);
        let (filter, _) = accepted(&cfg, &names[0]);
        assert!(accepts(&filter, &lan_src));

        // A list naming only some other network is no grant.
        cfg.networks.get_mut(&lan).unwrap().access = Some(vec!["nonexistent".into()]);
        let (filter, _) = accepted(&cfg, &names[0]);
        assert!(!accepts(&filter, &lan_src));
    }

    #[test]
    fn a_source_in_no_listed_network_is_never_accepted_and_nodes_always_are() {
        let cfg = cfg();
        let names: Vec<String> = cfg.nodes.keys().cloned().collect();
        let (filter, nat) = accepted(&cfg, &names[0]);
        assert!(!accepts(&filter, "203.0.113.77/32"));
        assert!(!filter.iter().any(|r| r.contains("203.0.113")));
        // Both implementing nodes' internal addresses are sources, whatever the lists say.
        for n in &names[..2] {
            let IpAddr::V4(ip) = cfg.internal_ip(cfg.node(n).unwrap()).unwrap() else {
                unreachable!()
            };
            assert!(accepts(&filter, &format!("{ip}/32")), "{filter:?}");
        }
        // Handed to another node, the source becomes this node's own internal address.
        let IpAddr::V4(own) = cfg.internal_ip(cfg.node(&names[0]).unwrap()).unwrap() else {
            unreachable!()
        };
        assert!(nat[2].ends_with(&format!("--to-source {own}")), "{nat:?}");
        // Through a tunnel it is the inner address.
        assert!(nat[1].contains("--to-source 169.254."), "{nat:?}");
        assert!(nat[0].ends_with("--to-source 192.168.32.1"), "{nat:?}");
        // A node implementing nothing renders no chain at all.
        let (filter, nat) = accepted(&cfg, &names[2]);
        assert!(filter.is_empty() && nat.is_empty());
    }

    #[test]
    fn routes_point_at_the_hosting_nodes_internal_address() {
        use cl_raft::ids::InstanceId;
        let cfg = cfg();
        let names: Vec<String> = cfg.nodes.keys().cloned().collect();
        let mut allocations = Allocations::default();
        let key = managed_key("mgd");
        allocations.sync_managed_network(
            &key,
            "192.168.32.0/24".parse().unwrap(),
            &cfg.managed_reserved("mgd"),
        );
        let remote = InstanceId::new("blog", "web", 0);
        let local = InstanceId::new("blog", "web", 1);
        let remote_ip = allocations.allocate_ip(&key, &remote).unwrap();
        allocations.allocate_ip(&key, &local).unwrap();
        let mut state = ClusterState {
            overlay: Some(serde_json::to_string(&allocations).unwrap()),
            ..Default::default()
        };
        for (id, node) in [(&remote, &names[1]), (&local, &names[0])] {
            state.assignments.insert(
                id.clone(),
                cl_raft::state::Assignment {
                    instance: id.clone(),
                    node: node.clone(),
                    desired: cl_raft::state::DesiredState::Running,
                    image_digest: None,
                    migration: None,
                },
            );
        }
        let routes = wanted_routes(&state, &cfg, &names[0]);
        assert_eq!(routes.len(), 1);
        let via = cfg.internal_ip(cfg.node(&names[1]).unwrap()).unwrap();
        assert_eq!(
            routes[&remote_ip],
            match via {
                IpAddr::V4(v) => v,
                _ => unreachable!(),
            }
        );
        // A node implementing nothing routes nothing.
        assert!(wanted_routes(&state, &cfg, &names[2]).is_empty());
    }
}
