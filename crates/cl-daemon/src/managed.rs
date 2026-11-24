//! Host firewall and open ports of managed private networks (see
//! `cl_net::managed`).
//!
//! Outside clients never reach a container address: they reach this node's
//! address on the network, and only the ports the network opens. Each open
//! port is a userspace forwarder (the same `cl_net::expose` machinery as
//! published ports) bound to that address, pointing at the container only
//! while that container runs on this node: a raw port is answered by the node
//! holding the container, nowhere else, and nothing here routes a container
//! address across nodes.
//!
//! Both halves are re-derived from the live config and replicated placement
//! every few seconds so a moved container, a node that left or an edited
//! `access` list takes effect without a restart. A node that implements no
//! managed network carries none of it.

use crate::supervisor::log_err;
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cl_config::{ClusterConfig, NetworkKind, Protocol};
use cl_net::expose::{serve_tcp, serve_udp, Routes};
use cl_net::iptables::{apply_plan, list_chains, remove_chain};
use cl_net::managed::port_veth_name;
use cl_net::managed::{
    managed_plan, remove_legacy_plumbing, ManagedInput, ManagedNetRules, FORWARD_CHAIN,
    INPUT_CHAIN, LEGACY_NAT_CHAIN, SNAT_CHAIN,
};
use cl_net::relay::Dialer;
use cl_net::vxlan::bridge_name;
use cl_net::{managed_key, Allocations};
use cl_raft::node::ClusterNode;
use cl_raft::state::{ClusterState, DesiredState};
use ipnet::{IpNet, Ipv4Net};
use tokio::task::JoinHandle;
use tracing::{info, warn};

const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// How long a forwarder waits before binding again after its listener
/// stopped, usually because something else already holds the port.
const BIND_RETRY: Duration = Duration::from_secs(30);

/// Subnets of the cluster networks whose traffic may reach managed network
/// `target`: a network without an `access` list is unrestricted (as for
/// `CLDR-NETF`), one with a list must name `target`. A source address in none of
/// them is in nobody's list and is dropped. A `container` network counts
/// through its routed range.
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

/// What this node must enforce for the managed networks it implements.
pub fn input_for(cfg: &ClusterConfig, node: &str) -> ManagedInput {
    let nets = crate::overlay::managed_local(cfg, node)
        .iter()
        .map(|m| {
            let mut ports: Vec<(Protocol, u16)> = cfg
                .managed_open_ports(&m.name)
                .iter()
                .map(|p| (p.protocol, p.port))
                .collect();
            if cfg.managed_proxy(&m.name) {
                ports.extend([(Protocol::Tcp, 80), (Protocol::Tcp, 443)]);
            }
            ManagedNetRules {
                name: m.name.clone(),
                subnet: m.subnet,
                bridge: bridge_name(&managed_key(&m.name)),
                port: port_veth_name(&managed_key(&m.name)),
                peers: m.peer_addrs.iter().copied().collect(),
                node_addr: m.node_addr,
                sources: allowed_sources(cfg, &m.name),
                ports,
            }
        })
        .collect();
    ManagedInput { nets }
}

/// The open ports this node answers, each with the container behind it. A
/// port whose container runs on another node (or nowhere) is absent: nothing
/// listens for it here. With several replicas on this node the lowest
/// instance is used, so a restart keeps choosing the same one.
pub fn open_ports(
    state: &ClusterState,
    cfg: &ClusterConfig,
    node: &str,
) -> BTreeMap<(Ipv4Addr, u16, Protocol), SocketAddr> {
    let mut out = BTreeMap::new();
    let Some(allocations) = Allocations::decode(state.overlay.as_deref()) else {
        return out;
    };
    for local in crate::overlay::managed_local(cfg, node) {
        let Some(addr) = local.node_addr else {
            continue;
        };
        let key = managed_key(&local.name);
        for open in cfg.managed_open_ports(&local.name) {
            let backend = state
                .assignments
                .values()
                .filter(|a| {
                    a.node == node
                        && a.desired == DesiredState::Running
                        && a.instance.stack == open.stack
                        && a.instance.service == open.service
                })
                .map(|a| &a.instance)
                .min()
                .and_then(|instance| allocations.instance_ip(&key, instance));
            if let Some(ip) = backend {
                out.entry((addr, open.port, open.protocol))
                    .or_insert_with(|| SocketAddr::from((ip, open.effective_target_port())));
            }
        }
    }
    out
}

fn remove_chains() -> Result<()> {
    for chain in [FORWARD_CHAIN, INPUT_CHAIN] {
        if list_chains("filter")?.iter().any(|c| c == chain) {
            // The chain goes together with its jump (see `remove_chain`).
            remove_chain("filter", chain)?;
            info!(chain, "removed the managed network chain");
        }
    }
    remove_snat()?;
    remove_legacy_nat()
}

/// The source NAT chain of forwarded traffic, gone when no peer is routed.
fn remove_snat() -> Result<()> {
    if list_chains("nat")?.iter().any(|c| c == SNAT_CHAIN) {
        remove_chain("nat", SNAT_CHAIN)?;
        info!(
            chain = SNAT_CHAIN,
            "removed the managed network source nat chain"
        );
    }
    Ok(())
}

/// The source NAT chain of earlier versions.
fn remove_legacy_nat() -> Result<()> {
    if list_chains("nat")?.iter().any(|c| c == LEGACY_NAT_CHAIN) {
        remove_chain("nat", LEGACY_NAT_CHAIN)?;
        info!(
            chain = LEGACY_NAT_CHAIN,
            "removed the managed network chain of an earlier version"
        );
    }
    Ok(())
}

fn apply(input: &ManagedInput) -> Result<()> {
    let plan = managed_plan(input);
    if plan.chains.is_empty() {
        return remove_chains();
    }
    apply_plan(&plan)?;
    // A node that lost its address keeps no input chain.
    if plan.chain(INPUT_CHAIN).is_none() && list_chains("filter")?.iter().any(|c| c == INPUT_CHAIN)
    {
        remove_chain("filter", INPUT_CHAIN)?;
    }
    if plan.chain(SNAT_CHAIN).is_none() {
        remove_snat()?;
    }
    remove_legacy_nat()
}

async fn firewall(cluster: Arc<ClusterNode>, node_name: String, boot_cfg: ClusterConfig) {
    let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);
    // Nothing to clean up once an empty pass has run; skips the iptables
    // calls on a cluster that never uses managed networks.
    let mut clean = false;
    // The routes and tunnels of earlier versions go once, at boot.
    log_err(
        "the legacy managed-network cleanup failed",
        tokio::task::spawn_blocking(remove_legacy_plumbing).await,
    );
    loop {
        ticker.tick().await;
        let state = cluster.snapshot().await;
        let cfg = crate::daemon::live_config(&state, &boot_cfg);
        let input = input_for(&cfg, &node_name);
        if input.nets.is_empty() && clean {
            continue;
        }
        let idle = input.nets.is_empty();
        let result = tokio::task::spawn_blocking(move || apply(&input))
            .await
            .context("managed network task panicked")
            .and_then(|r| r);
        match result {
            Ok(()) => clean = idle,
            Err(e) => warn!(error = %e, "cannot apply the managed network firewall"),
        }
    }
}

/// One listener on a node address and its route.
struct Listener {
    routes: Routes,
    task: JoinHandle<()>,
}

/// The listeners that are running; dropping them stops them all.
#[derive(Default)]
struct Listeners(BTreeMap<(Ipv4Addr, u16, Protocol), Listener>);

impl Drop for Listeners {
    fn drop(&mut self) {
        for ((_, port, _), listener) in &self.0 {
            listener.routes.set(*port, None);
            listener.task.abort();
        }
    }
}

async fn ports(cluster: Arc<ClusterNode>, node_name: String, boot_cfg: ClusterConfig) {
    let mut listening = Listeners::default();
    let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);
    loop {
        ticker.tick().await;
        let state = cluster.snapshot().await;
        let cfg = crate::daemon::live_config(&state, &boot_cfg);
        let desired = open_ports(&state, &cfg, &node_name);

        for ((addr, port, protocol), backend) in &desired {
            let key = (*addr, *port, *protocol);
            if let Some(existing) = listening.0.get(&key) {
                existing.routes.set(*port, Some(*backend));
                continue;
            }
            let routes = Routes::new();
            routes.set(*port, Some(*backend));
            let bind = SocketAddr::from((IpAddr::V4(*addr), *port));
            let (task_routes, port, protocol) = (routes.clone(), *port, *protocol);
            let task = tokio::spawn(async move {
                // Retried from inside the task: something else may hold the
                // port for a while.
                loop {
                    let served = match protocol {
                        Protocol::Tcp => {
                            serve_tcp(bind, port, task_routes.clone(), false, Dialer::direct())
                                .await
                        }
                        Protocol::Udp => {
                            serve_udp(bind, port, task_routes.clone(), Dialer::direct()).await
                        }
                    };
                    if let Err(e) = served {
                        warn!(%bind, %protocol, error = %e, "managed network open port stopped");
                    }
                    tokio::time::sleep(BIND_RETRY).await;
                }
            });
            listening.0.insert(key, Listener { routes, task });
            info!(%bind, %protocol, "opening a managed network port");
        }

        // The container left this node (or the port was dropped from the
        // config): stop answering and cut what is still open.
        listening.0.retain(|key, listener| {
            if desired.contains_key(key) {
                return true;
            }
            listener.routes.set(key.1, None);
            listener.task.abort();
            info!(addr = %key.0, port = key.1, protocol = %key.2, "closed a managed network port");
            false
        });
    }
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
        // One task, so aborting it drops both loops (and with them the listeners).
        tokio::join!(
            firewall(cluster.clone(), node_name.clone(), boot_cfg.clone()),
            ports(cluster, node_name, boot_cfg)
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::ids::InstanceId;

    fn cfg() -> ClusterConfig {
        let mut cfg = ClusterConfig::parse(include_str!("../../../examples/cluster.yaml")).unwrap();
        cfg.networks.insert(
            "mgd".into(),
            serde_yaml::from_str(
                "{ type: private, subnet: 192.168.32.0/24, managed: true, proxy: true, open_ports: [{ port: 2222, target_port: 22, stack: blog, service: web }, { port: 53, protocol: udp, stack: blog, service: web }] }",
            )
            .unwrap(),
        );
        for (i, node) in cfg.nodes.values_mut().take(2).enumerate() {
            node.network.insert(
                "mgd".into(),
                if i == 0 {
                    cl_config::NodeAddress::Ip("192.168.32.2".parse().unwrap())
                } else {
                    cl_config::NodeAddress::None
                },
            );
        }
        cfg
    }

    fn names(cfg: &ClusterConfig) -> Vec<String> {
        cfg.nodes.keys().cloned().collect()
    }

    /// The CLDR-MNG-IN rules a node renders for `mgd`.
    fn inbound(cfg: &ClusterConfig, node: &str) -> Vec<String> {
        managed_plan(&input_for(cfg, node))
            .chain(INPUT_CHAIN)
            .map(|c| c.rules.clone())
            .unwrap_or_default()
    }

    fn accepts(rules: &[String], source: &str) -> bool {
        rules
            .iter()
            .any(|r| r.contains(&format!("-s {source} ")) && r.ends_with("RETURN"))
    }

    #[test]
    fn only_nodes_implementing_a_managed_network_get_rules() {
        let cfg = cfg();
        let names = names(&cfg);
        let on = input_for(&cfg, &names[0]);
        assert_eq!(on.nets.len(), 1);
        assert_eq!(on.nets[0].node_addr, Some("192.168.32.2".parse().unwrap()));
        // The proxy flag opens 80/443 beside the open ports.
        assert!(on.nets[0].ports.contains(&(Protocol::Tcp, 2222)));
        assert!(on.nets[0].ports.contains(&(Protocol::Udp, 53)));
        assert!(on.nets[0].ports.contains(&(Protocol::Tcp, 443)));
        // An address-less listing gets the automatic one.
        let auto = input_for(&cfg, &names[1]);
        assert!(auto.nets[0].node_addr.is_some());
        assert_ne!(auto.nets[0].node_addr, on.nets[0].node_addr);
        assert!(input_for(&cfg, &names[2]).nets.is_empty());
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
        assert!(cfg.networks["tailnet"].kind == NetworkKind::Container);
        assert!(!allowed_sources(&cfg, "mgd").contains(&routed));
        cfg.networks.get_mut("tailnet").unwrap().access = Some(vec!["lan".into(), "mgd".into()]);
        assert!(allowed_sources(&cfg, "mgd").contains(&routed));
        cfg.networks.get_mut("tailnet").unwrap().access = None;
        assert!(allowed_sources(&cfg, "mgd").contains(&routed));
        let names = names(&cfg);
        assert!(accepts(&inbound(&cfg, &names[0]), "100.64.0.0/10"));
    }

    #[test]
    fn the_node_address_accepts_networks_without_an_access_list_and_those_that_list_the_network() {
        let mut cfg = cfg();
        let names = names(&cfg);
        let mut private = cfg
            .networks
            .iter()
            .filter(|(_, n)| n.kind == NetworkKind::Private && !n.managed)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        private.sort();
        let lan = private.first().cloned().expect("a private network");
        let lan_src = cfg.networks[&lan].subnet.unwrap().to_string();

        cfg.networks.get_mut(&lan).unwrap().access = None;
        let rules = inbound(&cfg, &names[0]);
        assert!(accepts(&rules, &lan_src), "{rules:?}");
        assert!(rules.last().unwrap().ends_with("DROP"));

        cfg.networks.get_mut(&lan).unwrap().access = Some(vec![]);
        assert!(!accepts(&inbound(&cfg, &names[0]), &lan_src));

        cfg.networks.get_mut(&lan).unwrap().access = Some(vec!["mgd".into()]);
        assert!(accepts(&inbound(&cfg, &names[0]), &lan_src));

        cfg.networks.get_mut(&lan).unwrap().access = Some(vec!["nonexistent".into()]);
        assert!(!accepts(&inbound(&cfg, &names[0]), &lan_src));
    }

    #[test]
    fn a_source_in_no_cluster_network_is_never_accepted() {
        let cfg = cfg();
        let rules = inbound(&cfg, &names(&cfg)[0]);
        assert!(!rules.iter().any(|r| r.contains("203.0.113")));
        assert!(rules.last().unwrap().ends_with("DROP"));
    }

    fn state(cfg: &ClusterConfig, placements: &[(u32, &str)]) -> ClusterState {
        let mut allocations = Allocations::default();
        let key = managed_key("mgd");
        allocations.sync_managed_network(
            &key,
            "192.168.32.0/24".parse().unwrap(),
            &cfg.managed_reserved("mgd"),
        );
        let mut state = ClusterState::default();
        for (index, node) in placements {
            let id = InstanceId::new("blog", "web", *index);
            allocations.allocate_ip(&key, &id).unwrap();
            state.assignments.insert(
                id.clone(),
                cl_raft::state::Assignment {
                    instance: id,
                    node: node.to_string(),
                    desired: DesiredState::Running,
                    image_digest: None,
                    migration: None,
                },
            );
        }
        state.overlay = Some(serde_json::to_string(&allocations).unwrap());
        state
    }

    #[test]
    fn a_port_is_answered_only_on_the_node_holding_the_container() {
        let cfg = cfg();
        let names = names(&cfg);
        let state = state(&cfg, &[(0, &names[1]), (1, &names[0]), (2, &names[0])]);
        let key = managed_key("mgd");
        let allocations = Allocations::decode(state.overlay.as_deref()).unwrap();
        let ip = |i: u32| {
            allocations
                .instance_ip(&key, &InstanceId::new("blog", "web", i))
                .unwrap()
        };

        let here = open_ports(&state, &cfg, &names[0]);
        let node_addr: Ipv4Addr = "192.168.32.2".parse().unwrap();
        // The lowest instance on this node, mapped to the target port.
        assert_eq!(
            here[&(node_addr, 2222, Protocol::Tcp)],
            SocketAddr::from((ip(1), 22))
        );
        assert_eq!(
            here[&(node_addr, 53, Protocol::Udp)],
            SocketAddr::from((ip(1), 53))
        );
        assert_eq!(here.len(), 2);
        // The other node holds instance 0: it answers on its own address.
        let there = open_ports(&state, &cfg, &names[1]);
        assert_eq!(there.len(), 2);
        assert!(there.keys().all(|(addr, _, _)| *addr != node_addr));
        assert!(there.values().all(|b| b.ip() == IpAddr::V4(ip(0))));
        // A node without the container, or a stopped one, answers nothing.
        assert!(open_ports(&state, &cfg, &names[2]).is_empty());
        let mut stopped = state.clone();
        for a in stopped.assignments.values_mut() {
            a.desired = DesiredState::Stopped;
        }
        assert!(open_ports(&stopped, &cfg, &names[0]).is_empty());
    }
}
