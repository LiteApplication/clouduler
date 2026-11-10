//! Container-kind networks on this node: the host port on the shared
//! segment, the policy routing that sends traffic through the router
//! container, and the firewall around the router's side.
//!
//! `overlay` builds the segment itself (bridge, VXLAN, docker network
//! `CTN_<net>`) and the leader allocates its addresses; this loop, every ten
//! seconds on every node, derives what `cl_net::container_net` should apply
//! from the live config and the replicated state, and applies it. Everything
//! is declarative and idempotent, so a router that moves, stops or comes
//! back needs nothing but the next pass.
//!
//! Fail-open/closed: host-level rules (the `hosts` list) are installed only
//! while the router runs; stack containers keep their routes (they fail
//! closed in their own namespace, see the scheduler).

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cl_api::container_router::{allocations_of, router_status, RouterState, RouterStatus};
use cl_config::{ClusterConfig, Protocol, StackFile};
use cl_net::container_net::{
    apply_routing, container_bridge_name, filter_iptables_plan, host_peer_name, host_port_mac,
    host_veth_name, l2_isolation_plan, mangle_iptables_plan, node_routing_plan, routing_ranks,
    FilterInput, RoutingInput, RoutingPlan,
};
use cl_net::iptables::apply_plan;
use cl_net::vxlan::{bridge_name, Overlay};
use cl_net::Allocations;
use cl_raft::node::ClusterNode;
use cl_raft::state::ClusterState;
use cl_sched::run::{overlay_name, overlay_network_name};
use ipnet::Ipv4Net;
use tracing::{debug, info, warn};

const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

/// What one node wants for one container network.
#[derive(Debug, Clone)]
pub struct Desired {
    pub network: String,
    pub bridge: String,
    pub veth: String,
    pub peer: String,
    pub mac: [u8; 6],
    pub node_ctn: Ipv4Addr,
    pub router: RouterStatus,
    pub routing: RoutingInput,
    pub open_ports: Vec<(u16, Protocol, u16)>,
    pub target_addr: Option<Ipv4Addr>,
    pub proxy_ports: Vec<u16>,
    pub allowed: Vec<Ipv4Net>,
    /// Every node's address on the segment.
    pub node_ips: Vec<Ipv4Addr>,
}

fn v4(ip: Option<IpAddr>) -> Option<Ipv4Addr> {
    match ip {
        Some(IpAddr::V4(ip)) => Some(ip),
        _ => None,
    }
}

/// Destinations the router side may forward to: the subnets of the allowed
/// networks that have one, plus every node's own address on each of them.
pub fn allowed_destinations(cfg: &ClusterConfig, allowed_networks: &[String]) -> Vec<Ipv4Net> {
    let mut out: BTreeSet<Ipv4Net> = BTreeSet::new();
    for name in allowed_networks {
        let Some(net) = cfg.networks.get(name) else {
            continue;
        };
        if let Some(ipnet::IpNet::V4(subnet)) = net.subnet {
            out.insert(subnet);
        }
        for node in cfg.nodes.values() {
            let ip = v4(node.ip_on(name)).or_else(|| node.public_on(name).and_then(|p| p.ipv4));
            if let Some(ip) = ip {
                out.insert(Ipv4Net::new(ip, 32).expect("/32 is valid"));
            }
        }
    }
    out.into_iter().collect()
}

/// Everything this node applies for `network`, or `None` while the leader has
/// not allocated the segment or this node's address yet.
pub fn desired(
    state: &ClusterState,
    cfg: &ClusterConfig,
    allocations: &Allocations,
    node: &str,
    network: &str,
    rank: u32,
) -> Option<Desired> {
    let (_, c) = cfg
        .container_networks()
        .find(|(name, _)| *name == network)?;
    let segment = cl_net::container_net::container_docker_name(network);
    allocations.subnet(&segment)?;
    let node_ctn = allocations.node_ip(&segment, node)?;
    let router = router_status(state, cfg, allocations, network)?;

    // Stacks on an allowed network that have an instance here get their
    // traffic towards the routed subnet sent through the router.
    let mut stack_sources: BTreeSet<Ipv4Net> = BTreeSet::new();
    let mut router_bridge = None;
    for (stack, stack_state) in &state.stacks {
        let Ok(file) = StackFile::parse(&stack_state.spec) else {
            continue;
        };
        let Some(overlay_net) = overlay_network_name(&file, cfg) else {
            continue;
        };
        let overlay = overlay_name(stack, overlay_net);
        if *stack == c.stack {
            router_bridge = Some(bridge_name(&overlay));
            continue;
        }
        if !c.allowed_networks.iter().any(|n| n == overlay_net) {
            continue;
        }
        let here = state
            .assignments
            .values()
            .any(|a| a.instance.stack == *stack && a.node == node);
        if here {
            if let Some(subnet) = allocations.subnet(&overlay) {
                stack_sources.insert(subnet);
            }
        }
    }

    let bridge = container_bridge_name(network);
    let veth = host_veth_name(network);
    let target_addr = cfg
        .node(node)
        .and_then(|n| cfg.internal_ip(n))
        .and_then(|ip| v4(Some(ip)));
    let proxy_ports = if c.proxy && cfg.proxy.enabled {
        vec![cfg.proxy.http_port, cfg.proxy.https_port]
    } else {
        Vec::new()
    };
    Some(Desired {
        network: network.to_string(),
        bridge,
        peer: host_peer_name(network),
        mac: host_port_mac(network, node),
        node_ctn,
        routing: RoutingInput {
            network: network.to_string(),
            subnet: c.subnet,
            router_ip: router.ip,
            router_bridge,
            node_ctn,
            rank,
            stack_sources: stack_sources.into_iter().collect(),
            host_listed: c.hosts.iter().any(|h| h == node),
            router_running: router.state == RouterState::Up,
        },
        veth,
        router,
        open_ports: c
            .open_ports
            .iter()
            .map(|p| (p.port, p.protocol, p.effective_target_port()))
            .collect(),
        target_addr,
        proxy_ports,
        allowed: allowed_destinations(cfg, &c.allowed_networks),
        node_ips: allocations.node_ips(&segment).map(|(_, ip)| ip).collect(),
    })
}

impl Desired {
    fn routing_plan(&self) -> RoutingPlan {
        node_routing_plan(&self.routing)
    }

    async fn apply_firewall(&self) -> Result<()> {
        let filter_input = (
            self.network.clone(),
            self.bridge.clone(),
            self.veth.clone(),
            self.router.ip,
            self.node_ctn,
            self.open_ports.clone(),
            self.target_addr,
            self.proxy_ports.clone(),
            self.allowed.clone(),
        );
        let (bridge, router_ip, node_ips) =
            (self.bridge.clone(), self.router.ip, self.node_ips.clone());
        let (network, veth) = (self.network.clone(), self.veth.clone());
        tokio::task::spawn_blocking(move || -> Result<()> {
            let (net, br, vt, rip, ctn, ports, target, proxy, allowed) = filter_input;
            let filter = filter_iptables_plan(&FilterInput {
                network: &net,
                bridge: &br,
                veth: &vt,
                router_ip: rip,
                node_ctn: ctn,
                open_ports: &ports,
                target_addr: target,
                proxy_ports: &proxy,
                allowed: &allowed,
            });
            apply_plan(&filter).context("container network filter")?;
            apply_plan(&mangle_iptables_plan(&network, &bridge, &veth, router_ip))
                .context("container network mangle")?;
            apply_plan(&l2_isolation_plan(&bridge, router_ip, &node_ips))
                .context("container network isolation")?;
            Ok(())
        })
        .await
        .context("iptables task panicked")?
    }
}

/// Remembers each router's last state so a change is logged once, not every
/// tick.
#[derive(Default)]
struct RouterWatch {
    last: BTreeMap<String, RouterState>,
}

impl RouterWatch {
    fn observe(&mut self, network: &str, router: &RouterStatus) {
        if self.last.get(network) == Some(&router.state) {
            return;
        }
        let first = !self.last.contains_key(network);
        self.last.insert(network.to_string(), router.state);
        match router.state {
            RouterState::Missing => warn!(
                network, stack = %router.stack, service = %router.service,
                "the router of this container network is not deployed or not placed; clients fail closed, listed hosts keep their normal route"
            ),
            RouterState::Down => warn!(
                network, instance = %router.instance, node = ?router.node,
                "the router of this container network is not running; clients fail closed, listed hosts keep their normal route"
            ),
            RouterState::Up if !first => info!(
                network, instance = %router.instance, node = ?router.node,
                "the router of this container network is running"
            ),
            RouterState::Up => {}
        }
    }

    fn retain(&mut self, networks: &BTreeSet<String>) {
        self.last.retain(|name, _| networks.contains(name));
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
        let netlink = match rtnetlink::new_connection() {
            Ok((connection, handle, _)) => {
                tokio::spawn(connection);
                Some(Overlay::new(handle))
            }
            Err(e) => {
                warn!(error = %e, "no netlink access, container networks will not come up");
                None
            }
        };
        let mut watch = RouterWatch::default();
        let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);
        loop {
            ticker.tick().await;
            let state = cluster.state().await;
            let cfg = crate::daemon::live_config(&state, &boot_cfg);
            if let Err(e) = reconcile(&state, &cfg, &node_name, netlink.as_ref(), &mut watch).await
            {
                warn!(error = %e, "container network reconcile failed");
            }
        }
    })
}

/// A plan that owns the network's tables but wants nothing in them.
fn empty_plan(network: &str) -> RoutingPlan {
    let (reply, dst) = cl_net::container_net::container_tables(network);
    RoutingPlan {
        tables: [reply, dst].into(),
        ..RoutingPlan::default()
    }
}

async fn reconcile(
    state: &ClusterState,
    cfg: &ClusterConfig,
    node: &str,
    netlink: Option<&Overlay>,
    watch: &mut RouterWatch,
) -> Result<()> {
    let names: BTreeSet<String> = cfg.container_networks().map(|(n, _)| n.clone()).collect();
    watch.retain(&names);
    if names.is_empty() {
        // Whatever a removed network left behind is the egress sweep's job.
        return Ok(());
    }
    let allocations = allocations_of(state);
    let ranks = routing_ranks(
        cfg.container_networks()
            .map(|(n, c)| (n.as_str(), c.subnet)),
    );

    let mut merged = RoutingPlan::default();
    for network in &names {
        let rank = ranks.get(network).copied().unwrap_or(0);
        let Some(d) = desired(state, cfg, &allocations, node, network, rank) else {
            debug!(network, "container network not allocated for this node yet");
            // Still manage its tables (empty), so stale rules and routes go.
            merged.extend(empty_plan(network));
            continue;
        };
        watch.observe(network, &d.router);

        let Some(netlink) = netlink else { continue };
        if let Err(e) = netlink
            .ensure_host_port(&d.bridge, &d.veth, &d.peer, d.mac, d.node_ctn)
            .await
        {
            // The segment's bridge is built by the overlay loop; until it
            // exists there is nothing to attach to.
            debug!(network, error = %e, "cannot bring up the container network host port yet");
            merged.extend(empty_plan(network));
            continue;
        }
        if let Err(e) = d.apply_firewall().await {
            warn!(network, error = %e, "cannot apply the container network firewall");
        }
        merged.extend(d.routing_plan());
    }

    if merged.tables.is_empty() {
        return Ok(());
    }
    tokio::task::spawn_blocking(move || apply_routing(&merged))
        .await
        .context("routing task panicked")?
        .context("applying the container network routing")
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_net::container_net::{container_docker_name, RuleSelector};
    use cl_raft::ids::InstanceId;
    use cl_raft::state::{Assignment, ContainerStatus, Request};

    fn cfg() -> ClusterConfig {
        ClusterConfig::parse(include_str!("../../../examples/cluster.yaml")).unwrap()
    }

    fn assign(state: &mut ClusterState, stack: &str, service: &str, node: &str) {
        let instance = InstanceId::new(stack, service, 0);
        state.assignments.insert(
            instance.clone(),
            Assignment {
                instance,
                node: node.into(),
                desired: cl_raft::DesiredState::Running,
                image_digest: None,
                migration: None,
            },
        );
    }

    /// A cluster with the router on n2, a client stack on n1 and a stack on
    /// the allowed `lan` network on n1.
    fn world() -> (ClusterState, Allocations) {
        let mut state = ClusterState::default();
        let spec = |body: &str| body.to_string();
        for (name, body) in [
            (
                "tailscale-router",
                "services:\n  tailscale:\n    image: x\n",
            ),
            (
                "lanapp",
                "networks:\n  lan: {}\nservices:\n  app:\n    image: x\n    networks: [lan]\n",
            ),
        ] {
            state.apply(&Request::DeployStack {
                name: name.into(),
                spec: spec(body),
                at: 0,
            });
        }
        assign(&mut state, "tailscale-router", "tailscale", "n2");
        assign(&mut state, "lanapp", "app", "n1");
        let mut alloc = allocations_of(&state);
        let seg = container_docker_name("tailnet");
        alloc.allocate_network(&seg, &[]).unwrap();
        for n in ["n1", "n2", "n3"] {
            alloc.allocate_node_ip(&seg, n).unwrap();
        }
        alloc
            .allocate_ip(&seg, &InstanceId::new("tailscale-router", "tailscale", 0))
            .unwrap();
        alloc.allocate_network("lanapp_lan", &[]).unwrap();
        (state, alloc)
    }

    #[test]
    fn nothing_is_planned_until_the_segment_and_node_address_exist() {
        let (state, _) = world();
        let empty = allocations_of(&ClusterState::default());
        assert!(desired(&state, &cfg(), &empty, "n1", "tailnet", 0).is_none());
        assert!(desired(&state, &cfg(), &allocations_of(&state), "n1", "lan", 0).is_none());
    }

    #[test]
    fn a_stopped_router_leaves_hosts_alone_but_keeps_stack_sources() {
        let (state, alloc) = world();
        let d = desired(&state, &cfg(), &alloc, "n1", "tailnet", 0).unwrap();
        assert_eq!(d.router.state, RouterState::Down);
        assert!(!d.routing.router_running);
        assert!(d.routing.host_listed, "n1 is in hosts");
        let plan = d.routing_plan();
        assert!(!plan.rules.iter().any(|r| r.selector == RuleSelector::All));
        assert_eq!(
            d.routing.stack_sources,
            vec![alloc.subnet("lanapp_lan").unwrap()]
        );
        assert!(plan
            .rules
            .iter()
            .any(|r| matches!(&r.selector, RuleSelector::From(n) if *n == alloc.subnet("lanapp_lan").unwrap())));
    }

    #[test]
    fn a_running_router_turns_the_host_rules_on_only_for_listed_hosts() {
        let (mut state, alloc) = world();
        state.container_status.insert(
            InstanceId::new("tailscale-router", "tailscale", 0),
            ContainerStatus {
                node: "n2".into(),
                running: true,
                secret_revisions: Default::default(),
            },
        );
        let listed = desired(&state, &cfg(), &alloc, "n1", "tailnet", 0).unwrap();
        assert!(listed.routing.router_running);
        assert!(listed
            .routing_plan()
            .rules
            .iter()
            .any(|r| r.selector == RuleSelector::All));
        // n2 is not in `hosts`, and has no stack on `lan`.
        let unlisted = desired(&state, &cfg(), &alloc, "n2", "tailnet", 0).unwrap();
        assert!(unlisted.routing.stack_sources.is_empty());
        assert!(!unlisted
            .routing_plan()
            .rules
            .iter()
            .any(|r| r.selector == RuleSelector::All));
        assert_eq!(unlisted.node_ips.len(), 3);
    }

    #[test]
    fn open_ports_proxy_ports_and_destinations_follow_the_config() {
        let (state, alloc) = world();
        let c = cfg();
        let d = desired(&state, &c, &alloc, "n1", "tailnet", 0).unwrap();
        assert_eq!(d.open_ports, vec![(2222, Protocol::Tcp, 22)]);
        assert_eq!(d.proxy_ports, vec![c.proxy.http_port, c.proxy.https_port]);
        assert_eq!(d.target_addr, Some("10.10.0.1".parse().unwrap()));
        // The `lan` subnet and every node's address on it.
        assert!(d.allowed.contains(&"192.168.1.0/24".parse().unwrap()));
        assert!(d.allowed.iter().any(|n| n.prefix_len() == 32
            && "192.168.1.0/24"
                .parse::<Ipv4Net>()
                .unwrap()
                .contains(&n.addr())));
        let mut no_proxy = c.clone();
        no_proxy
            .networks
            .get_mut("tailnet")
            .unwrap()
            .container
            .as_mut()
            .unwrap()
            .proxy = false;
        assert!(desired(&state, &no_proxy, &alloc, "n1", "tailnet", 0)
            .unwrap()
            .proxy_ports
            .is_empty());
    }

    #[test]
    fn a_router_change_is_logged_once() {
        let mut watch = RouterWatch::default();
        let (state, alloc) = world();
        let d = desired(&state, &cfg(), &alloc, "n1", "tailnet", 0).unwrap();
        watch.observe("tailnet", &d.router);
        assert_eq!(watch.last["tailnet"], RouterState::Down);
        watch.observe("tailnet", &d.router);
        let mut up = d.router.clone();
        up.state = RouterState::Up;
        watch.observe("tailnet", &up);
        assert_eq!(watch.last["tailnet"], RouterState::Up);
        watch.retain(&BTreeSet::new());
        assert!(watch.last.is_empty());
    }
}
