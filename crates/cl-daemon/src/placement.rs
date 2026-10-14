//! Where every container currently sits on the overlay.
//!
//! Internal DNS, published ports and the reverse proxy all answer the same
//! question first: which addresses back this service right now. That answer
//! comes from replicated state alone (assignments plus the overlay address
//! book), never from the local docker, so every node reaches the same
//! conclusion about containers it is not running.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use cl_config::{ClusterConfig, ServiceExtensions, StackFile};
use cl_net::relay::relay_port;
use cl_net::Allocations;
use cl_raft::ids::InstanceId;
use cl_raft::state::{ClusterState, DesiredState};
use cl_sched::run::{overlay_name, overlay_network_name};

/// One service's declared publishing and the addresses currently backing it.
#[derive(Debug, Clone, PartialEq)]
pub struct ServicePlacement {
    pub stack: String,
    pub service: String,
    pub extensions: ServiceExtensions,
    /// One address per instance meant to be running, in instance order so
    /// every node builds the same backend list.
    pub addresses: Vec<Ipv4Addr>,
}

/// Every instance that has an overlay address, in instance order.
pub fn instance_addresses(
    state: &ClusterState,
    cfg: &ClusterConfig,
) -> Vec<(InstanceId, Ipv4Addr)> {
    let Some(allocations) = Allocations::decode(state.overlay.as_deref()) else {
        return Vec::new();
    };

    let mut out = BTreeMap::new();
    for (network, stack) in networks_by_stack(state, cfg) {
        for (instance, ip) in allocations.instances(&network) {
            // An address the leader has not reclaimed yet still points at a
            // container that is gone. Assignments are the authority.
            if instance.stack != stack || !wants_running(state, instance) {
                continue;
            }
            out.insert(instance.clone(), ip);
        }
    }
    out.into_iter().collect()
}

/// Every service that publishes something, with its current backends.
pub fn service_placements(state: &ClusterState, cfg: &ClusterConfig) -> Vec<ServicePlacement> {
    let addresses: BTreeMap<InstanceId, Ipv4Addr> =
        instance_addresses(state, cfg).into_iter().collect();
    let mut out = Vec::new();

    for (name, stack) in &state.stacks {
        let Ok(file) = StackFile::parse(&stack.spec) else {
            continue;
        };
        for (service, spec) in &file.services {
            let Some(extensions) = &spec.x_clouduler else {
                continue;
            };
            if extensions.proxy.is_empty() && extensions.expose.is_empty() {
                continue;
            }
            out.push(ServicePlacement {
                stack: name.clone(),
                service: service.clone(),
                extensions: extensions.clone(),
                addresses: addresses
                    .iter()
                    .filter(|(id, _)| id.stack == *name && id.service == *service)
                    .map(|(_, ip)| *ip)
                    .collect(),
            });
        }
    }
    out
}

/// The overlay network name each stack's containers sit on. Matches how the
/// scheduler picks one (`cl_sched::run::overlay_network_name`: the declared
/// network of subnet-addressed kind, not just whichever sorts first), so the
/// addresses looked up here are the addresses the containers actually got.
/// A stack with no such network (public/egress/wireguard only) contributes
/// nothing here, same as it gets no overlay built for it at all.
fn networks_by_stack(state: &ClusterState, cfg: &ClusterConfig) -> Vec<(String, String)> {
    state
        .stacks
        .iter()
        .filter_map(|(name, stack)| {
            let file = StackFile::parse(&stack.spec).ok()?;
            let network = overlay_network_name(&file, cfg)?;
            Some((overlay_name(name, network), name.clone()))
        })
        .collect()
}

/// Nodes currently running an instance of `stack`/`service`, from
/// assignments rather than the overlay address book: DNS needs to know where
/// the container actually runs, not just its overlay address.
pub fn service_nodes(state: &ClusterState, stack: &str, service: &str) -> BTreeSet<String> {
    state
        .assignments
        .iter()
        .filter(|(id, _)| id.stack == stack && id.service == service && wants_running(state, id))
        .map(|(_, a)| a.node.clone())
        .collect()
}

/// How `local_node` reaches every container another node runs: container
/// address -> the relay endpoint (`cl_net::relay`) of the node running it,
/// on that node's internal address. This host cannot open a connection to a
/// container on another node over the overlay itself (every bridge holds the
/// same anycast gateway), so the proxy, its health checks and the expose
/// forwarders dial those through the relay. A container this node runs is
/// left out and keeps the direct path, as is one on a node the config gives
/// no internal address: nothing better than a direct dial exists for it.
pub fn relay_routes(
    state: &ClusterState,
    cfg: &ClusterConfig,
    local_node: &str,
) -> HashMap<IpAddr, SocketAddr> {
    instance_addresses(state, cfg)
        .into_iter()
        .filter_map(|(instance, ip)| {
            let owner = &state.assignments.get(&instance)?.node;
            if owner == local_node {
                return None;
            }
            let node = cfg.node(owner)?;
            let relay = SocketAddr::new(cfg.internal_ip(node)?, relay_port(node.rpc_port));
            Some((IpAddr::V4(ip), relay))
        })
        .collect()
}

fn wants_running(state: &ClusterState, instance: &InstanceId) -> bool {
    state
        .assignments
        .get(instance)
        .is_some_and(|a| a.desired == DesiredState::Running)
}

#[cfg(test)]
pub mod fixtures {
    use super::*;
    use cl_net::ipam::{DEFAULT_OVERLAY_POOL, DEFAULT_SUBNET_PREFIX};
    use cl_raft::state::{Assignment, StackState};

    pub const STACK: &str = "blog";

    /// A stack with `replicas` instances of `web`, all placed and addressed.
    pub fn state_with(spec: &str, replicas: u32) -> ClusterState {
        let mut state = ClusterState {
            stacks: BTreeMap::from([(
                STACK.to_string(),
                StackState {
                    name: STACK.to_string(),
                    spec: spec.to_string(),
                    revision: 1,
                    configs: Default::default(),
                    secrets: Default::default(),
                    used_configs: Default::default(),
                    used_secrets: Default::default(),
                },
            )]),
            ..Default::default()
        };

        let network = overlay_name(STACK, "default");
        let mut allocations = Allocations::new(
            DEFAULT_OVERLAY_POOL.parse().expect("pool parses"),
            DEFAULT_SUBNET_PREFIX,
        );
        allocations
            .allocate_network(&network, &[])
            .expect("allocates");

        for index in 0..replicas {
            let instance = InstanceId::new(STACK, "web", index);
            allocations
                .allocate_ip(&network, &instance)
                .expect("allocates");
            state.assignments.insert(
                instance.clone(),
                Assignment {
                    instance,
                    node: "n1".into(),
                    desired: DesiredState::Running,
                    image_digest: None,
                    migration: None,
                },
            );
        }

        state.overlay = Some(serde_json::to_string(&allocations).expect("encodes"));
        state
    }

    /// A `ClusterConfig` whose "default" network is `internal` kind, matching
    /// `SPEC`'s network so `overlay_network_name` picks it the same way the
    /// scheduler and overlay builder do.
    pub fn cluster_config() -> ClusterConfig {
        ClusterConfig::parse(
            "\
cluster: { name: test }
networks:
  default: { type: internal, subnet: 10.10.0.0/24, domain: int.example.com }
  wan: { type: egress }
nodes:
  n1:
    id: 3b0f6c4a-3d3f-4a8e-9b8b-2c1e6a2a9f01
    role: manager
    network: { default: 10.10.0.1, wan: eth0 }
web: { address: cloud, network: default }
",
        )
        .expect("valid cluster config")
    }

    pub const SPEC: &str = "\
networks:
  default: {}
services:
  web:
    image: nginx
    networks: [default]
    x-clouduler:
      proxy:
        - host: blog
          network: wan
          port: 80
      expose:
        - port: 22
          published: 2222
          network: wan
";
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn every_running_instance_gets_an_address() {
        let state = state_with(SPEC, 2);
        let placed = instance_addresses(&state, &cluster_config());
        assert_eq!(placed.len(), 2);
        assert_ne!(placed[0].1, placed[1].1);
    }

    /// The address book lags the assignments by up to a reconcile pass, and a
    /// stale address points at a container that is no longer there.
    #[test]
    fn an_instance_without_an_assignment_is_not_published() {
        let mut state = state_with(SPEC, 2);
        state.assignments.remove(&InstanceId::new(STACK, "web", 1));
        assert_eq!(instance_addresses(&state, &cluster_config()).len(), 1);
    }

    #[test]
    fn a_stopped_instance_is_not_published() {
        let mut state = state_with(SPEC, 1);
        let instance = InstanceId::new(STACK, "web", 0);
        state
            .assignments
            .get_mut(&instance)
            .expect("assigned")
            .desired = DesiredState::Stopped;
        assert!(instance_addresses(&state, &cluster_config()).is_empty());
    }

    #[test]
    fn a_service_carries_every_replica_as_a_backend() {
        let placements = service_placements(&state_with(SPEC, 3), &cluster_config());
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].service, "web");
        assert_eq!(placements[0].addresses.len(), 3);
        assert_eq!(placements[0].extensions.proxy[0].port, 80);
    }

    /// Only services that publish something are of interest to the proxy and
    /// the port forwarders.
    #[test]
    fn a_service_publishing_nothing_is_left_out() {
        let spec = "networks:\n  default: {}\nservices:\n  web:\n    image: nginx\n";
        assert!(service_placements(&state_with(spec, 1), &cluster_config()).is_empty());
    }

    /// The real bug this module used to have: `networks_by_stack` picked
    /// whichever network sorted first out of a stack's *declared* networks,
    /// not the one actually carrying the overlay - so a stack whose networks
    /// happened to sort a public/egress one before its internal one got no
    /// addresses at all, even though its containers were running fine.
    #[test]
    fn a_network_that_sorts_before_the_internal_one_does_not_hide_the_addresses() {
        let spec = "\
networks:
  aegress: {}
  default: {}
services:
  web:
    image: nginx
    networks: [aegress, default]
    x-clouduler:
      proxy:
        - host: blog
          network: aegress
          port: 80
";
        let mut cfg = cluster_config();
        cfg.networks.insert(
            "aegress".to_string(),
            cl_config::Network {
                kind: cl_config::NetworkKind::Egress,
                subnet: None,
                domain: None,
            },
        );
        let state = state_with(spec, 1);
        assert_eq!(instance_addresses(&state, &cfg).len(), 1);
    }

    fn two_node_config() -> ClusterConfig {
        ClusterConfig::parse(
            "\
cluster: { name: test }
networks:
  default: { type: internal, subnet: 10.10.0.0/24, domain: int.example.com }
  wan: { type: egress }
nodes:
  n1:
    id: 3b0f6c4a-3d3f-4a8e-9b8b-2c1e6a2a9f01
    role: manager
    network: { default: 10.10.0.1, wan: eth0 }
  n2:
    id: 7d1e2f3a-4b5c-4d6e-8f70-81a2b3c4d5e6
    role: worker
    rpc_port: 8000
    network: { default: 10.10.0.2, wan: eth0 }
web: { address: cloud, network: default }
",
        )
        .expect("valid cluster config")
    }

    fn move_to(state: &mut ClusterState, index: u32, node: &str) {
        state
            .assignments
            .get_mut(&InstanceId::new(STACK, "web", index))
            .expect("assigned")
            .node = node.to_string();
    }

    /// The anycast gateway keeps a host from reaching another node's
    /// container over the overlay, so those - and only those - go through
    /// the owning node's relay, on its internal address.
    #[test]
    fn only_containers_on_other_nodes_go_through_their_nodes_relay() {
        let cfg = two_node_config();
        let mut state = state_with(SPEC, 2);
        move_to(&mut state, 1, "n2");
        let addresses: BTreeMap<InstanceId, Ipv4Addr> =
            instance_addresses(&state, &cfg).into_iter().collect();
        let local = IpAddr::V4(addresses[&InstanceId::new(STACK, "web", 0)]);
        let remote = IpAddr::V4(addresses[&InstanceId::new(STACK, "web", 1)]);

        let from_n1 = relay_routes(&state, &cfg, "n1");
        assert_eq!(
            from_n1,
            HashMap::from([(remote, "10.10.0.2:8002".parse().unwrap())])
        );

        let from_n2 = relay_routes(&state, &cfg, "n2");
        assert_eq!(from_n2.len(), 1);
        assert_eq!(
            from_n2[&local],
            SocketAddr::new(
                "10.10.0.1".parse().unwrap(),
                relay_port(cfg.nodes["n1"].rpc_port)
            )
        );
    }

    /// No relay to go through for a node the config does not know: a
    /// direct dial is the only thing left to try.
    #[test]
    fn a_container_on_an_unknown_node_is_dialed_directly() {
        let mut state = state_with(SPEC, 1);
        move_to(&mut state, 0, "gone");
        assert!(relay_routes(&state, &two_node_config(), "n1").is_empty());
    }

    #[test]
    fn a_stopped_container_on_another_node_needs_no_relay() {
        let mut state = state_with(SPEC, 1);
        move_to(&mut state, 0, "n2");
        state
            .assignments
            .get_mut(&InstanceId::new(STACK, "web", 0))
            .expect("assigned")
            .desired = DesiredState::Stopped;
        assert!(relay_routes(&state, &two_node_config(), "n1").is_empty());
    }
}
