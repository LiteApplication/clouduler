//! Where every container currently sits on the overlay.
//!
//! Internal DNS, published ports and the reverse proxy all answer the same
//! question first: which addresses back this service right now. That answer
//! comes from replicated state alone (assignments plus the overlay address
//! book), never from the local docker, so every node reaches the same
//! conclusion about containers it is not running.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use cl_config::{ServiceExtensions, StackFile};
use cl_net::Allocations;
use cl_raft::ids::InstanceId;
use cl_raft::state::{ClusterState, DesiredState};
use cl_sched::run::overlay_name;

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
pub fn instance_addresses(state: &ClusterState) -> Vec<(InstanceId, Ipv4Addr)> {
    let Some(allocations) = Allocations::decode(state.overlay.as_deref()) else {
        return Vec::new();
    };

    let mut out = BTreeMap::new();
    for (network, stack) in networks_by_stack(state) {
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
pub fn service_placements(state: &ClusterState) -> Vec<ServicePlacement> {
    let addresses: BTreeMap<InstanceId, Ipv4Addr> = instance_addresses(state).into_iter().collect();
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
/// scheduler picks one, so the addresses looked up here are the addresses the
/// containers actually got.
fn networks_by_stack(state: &ClusterState) -> Vec<(String, String)> {
    state
        .stacks
        .iter()
        .filter_map(|(name, stack)| {
            let file = StackFile::parse(&stack.spec).ok()?;
            let network = file.networks.keys().next()?;
            Some((overlay_name(name, network), name.clone()))
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
pub(crate) mod fixtures {
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
                },
            )]),
            ..Default::default()
        };

        let network = overlay_name(STACK, "default");
        let mut allocations = Allocations::new(
            DEFAULT_OVERLAY_POOL.parse().expect("pool parses"),
            DEFAULT_SUBNET_PREFIX,
        );
        allocations.allocate_network(&network, &[]).expect("allocates");

        for index in 0..replicas {
            let instance = InstanceId::new(STACK, "web", index);
            allocations.allocate_ip(&network, &instance).expect("allocates");
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
        let placed = instance_addresses(&state);
        assert_eq!(placed.len(), 2);
        assert_ne!(placed[0].1, placed[1].1);
    }

    /// The address book lags the assignments by up to a reconcile pass, and a
    /// stale address points at a container that is no longer there.
    #[test]
    fn an_instance_without_an_assignment_is_not_published() {
        let mut state = state_with(SPEC, 2);
        state.assignments.remove(&InstanceId::new(STACK, "web", 1));
        assert_eq!(instance_addresses(&state).len(), 1);
    }

    #[test]
    fn a_stopped_instance_is_not_published() {
        let mut state = state_with(SPEC, 1);
        let instance = InstanceId::new(STACK, "web", 0);
        state.assignments.get_mut(&instance).expect("assigned").desired = DesiredState::Stopped;
        assert!(instance_addresses(&state).is_empty());
    }

    #[test]
    fn a_service_carries_every_replica_as_a_backend() {
        let placements = service_placements(&state_with(SPEC, 3));
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
        assert!(service_placements(&state_with(spec, 1)).is_empty());
    }
}
