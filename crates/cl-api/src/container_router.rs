//! The state of a container network's router container, derived from the
//! replicated state. One definition for the daemon's warnings, the problem
//! list and the network detail endpoint.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use cl_config::ClusterConfig;
use cl_net::container_net::container_docker_name;
use cl_net::Allocations;
use cl_raft::ids::InstanceId;
use cl_raft::state::ClusterState;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RouterState {
    /// No instance of the router service is assigned (stack or service not
    /// deployed, or not placed yet).
    Missing,
    /// Assigned, but not running on its node.
    Down,
    Up,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouterStatus {
    pub stack: String,
    pub service: String,
    pub instance: String,
    pub node: Option<String>,
    pub ip: Option<Ipv4Addr>,
    pub running: bool,
    pub state: RouterState,
}

/// The router of `network`, or `None` when it is not a container network.
pub fn router_status(
    state: &ClusterState,
    cfg: &ClusterConfig,
    allocations: &Allocations,
    network: &str,
) -> Option<RouterStatus> {
    let (_, c) = cfg
        .container_networks()
        .find(|(name, _)| *name == network)?;
    let id = InstanceId::new(c.stack.clone(), c.service.clone(), 0);
    let node = state.assignments.get(&id).map(|a| a.node.clone());
    let running = node.as_ref().is_some_and(|node| {
        state
            .container_status
            .get(&id)
            .is_some_and(|s| s.running && s.node == *node)
    });
    let ip = allocations.instance_ip(&container_docker_name(network), &id);
    let state = match (&node, running) {
        (None, _) => RouterState::Missing,
        (Some(_), false) => RouterState::Down,
        (Some(_), true) => RouterState::Up,
    };
    Some(RouterStatus {
        stack: c.stack.clone(),
        service: c.service.clone(),
        instance: id.to_string(),
        node,
        ip,
        running,
        state,
    })
}

/// Decode the allocation book out of the replicated state.
pub fn allocations_of(state: &ClusterState) -> Allocations {
    Allocations::decode(state.overlay.as_deref()).unwrap_or_else(|| {
        Allocations::new(
            cl_net::ipam::DEFAULT_OVERLAY_POOL
                .parse()
                .expect("the default pool parses"),
            cl_net::ipam::DEFAULT_SUBNET_PREFIX,
        )
    })
}

/// The status of every container network.
pub fn all_statuses(state: &ClusterState, cfg: &ClusterConfig) -> BTreeMap<String, RouterStatus> {
    let allocations = allocations_of(state);
    cfg.container_networks()
        .filter_map(|(name, _)| {
            Some((name.clone(), router_status(state, cfg, &allocations, name)?))
        })
        .collect()
}

/// The config carried in the replicated state, if any.
pub fn config_of(state: &ClusterState) -> Option<ClusterConfig> {
    ClusterConfig::parse(&state.cluster_config.as_ref()?.yaml).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::ContainerStatus;

    fn cfg() -> ClusterConfig {
        ClusterConfig::parse(include_str!("../../../examples/cluster.yaml")).unwrap()
    }

    fn assign(state: &mut ClusterState) {
        state.assignments.insert(
            InstanceId::new("tailscale-router", "tailscale", 0),
            cl_raft::state::Assignment {
                instance: InstanceId::new("tailscale-router", "tailscale", 0),
                node: "n1".into(),
                desired: cl_raft::DesiredState::Running,
                image_digest: None,
                migration: None,
            },
        );
    }

    #[test]
    fn missing_down_up() {
        let cfg = cfg();
        let mut state = ClusterState::default();
        let alloc = allocations_of(&state);
        let s = router_status(&state, &cfg, &alloc, "tailnet").unwrap();
        assert_eq!(s.state, RouterState::Missing);
        assert_eq!(s.instance, "tailscale-router.tailscale.0");

        assign(&mut state);
        let s = router_status(&state, &cfg, &alloc, "tailnet").unwrap();
        assert_eq!(
            (s.state, s.node.as_deref()),
            (RouterState::Down, Some("n1"))
        );

        let id = InstanceId::new("tailscale-router", "tailscale", 0);
        let status = |running, node: &str| ContainerStatus {
            node: node.into(),
            running,
            secret_revisions: Default::default(),
        };
        state
            .container_status
            .insert(id.clone(), status(true, "n2"));
        // Reported by a node that is not the assigned one: not running there.
        assert_eq!(
            router_status(&state, &cfg, &alloc, "tailnet")
                .unwrap()
                .state,
            RouterState::Down
        );
        state.container_status.insert(id, status(true, "n1"));
        let s = router_status(&state, &cfg, &alloc, "tailnet").unwrap();
        assert_eq!(s.state, RouterState::Up);
        assert!(s.running);
        assert!(router_status(&state, &cfg, &alloc, "lan").is_none());
    }
}
