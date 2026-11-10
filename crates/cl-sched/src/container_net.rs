//! What a container needs to sit on a `container`-kind network: the pinned
//! address on the cluster-wide segment and the routes pushed into its network
//! namespace. Pure, so it is unit-tested without docker.
//!
//! The router's own address is never part of a client's identity (see
//! `run::revision_with_mounts`): only the client's address on the segment is.
//! Routes are re-applied every tick instead, so a router appearing or moving
//! never recreates its clients.

use std::net::{IpAddr, Ipv4Addr};

use cl_config::{ClusterConfig, NetworkKind};
use cl_docker::ContainerAttachment;
use cl_net::container_net::{client_netns_routes, container_docker_name, router_netns_routes};
use cl_net::Allocations;
use cl_raft::ids::InstanceId;

/// Interface name asked of docker for the router's attachment.
pub const ROUTER_IFNAME: &str = "ctn0";

/// Attachments for one instance, plus whether it is a router (needs IP
/// forwarding).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerPlan {
    pub attachments: Vec<ContainerAttachment>,
    pub router: bool,
}

/// Why an instance cannot be planned yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending(pub String);

/// Plan the container-network attachments of `instance`, which runs on
/// `local_node`. `client_of` lists the container networks its service names
/// (`StackFile::container_networks`). `None` from `Err(Pending)` means an
/// address is not allocated yet: skip the instance this pass.
pub fn plan(
    instance: &InstanceId,
    client_of: &[String],
    cfg: &ClusterConfig,
    allocations: Option<&Allocations>,
    local_node: &str,
) -> Result<ContainerPlan, Pending> {
    let mut out = ContainerPlan::default();

    for net in client_of {
        let Some(c) = cfg.networks.get(net).and_then(|n| n.container.as_ref()) else {
            continue;
        };
        // A router is attached to its own network below, never as a client.
        if cfg.container_router_of(&instance.stack, &instance.service) == Some(net.as_str()) {
            continue;
        }
        let key = container_docker_name(net);
        let ip = allocations
            .and_then(|a| a.instance_ip(&key, instance))
            .ok_or_else(|| Pending(format!("no address on container network {net} yet")))?;
        let router_ip = allocations
            .and_then(|a| a.instance_ip(&key, &InstanceId::new(&c.stack, &c.service, 0)));
        out.attachments.push(ContainerAttachment {
            docker_network: key,
            ip,
            routes: client_netns_routes(c.subnet, router_ip),
            ifname: None,
            default_route: c.subnet.prefix_len() == 0,
        });
    }

    if let Some(net) = cfg.container_router_of(&instance.stack, &instance.service) {
        let c = cfg.networks[net]
            .container
            .as_ref()
            .expect("router_of only names networks with a container block");
        let key = container_docker_name(net);
        let ip = allocations
            .and_then(|a| a.instance_ip(&key, instance))
            .ok_or_else(|| Pending(format!("no address on container network {net} yet")))?;

        let mut node_dests: Vec<(Ipv4Addr, Ipv4Addr)> = Vec::new();
        let mut subnets = Vec::new();
        for allowed in &c.allowed_networks {
            let Some(network) = cfg.networks.get(allowed) else {
                continue;
            };
            if network.kind.is_subnetted() {
                if let Some(ipnet::IpNet::V4(subnet)) = network.subnet {
                    subnets.push(subnet);
                }
            }
            for (node_name, node) in &cfg.nodes {
                let Some(via) = allocations.and_then(|a| a.node_ip(&key, node_name)) else {
                    continue;
                };
                let dest = if network.kind.is_subnetted() {
                    match node.ip_on(allowed) {
                        Some(IpAddr::V4(ip)) => Some(ip),
                        _ => None,
                    }
                } else if network.kind == NetworkKind::Public {
                    node.public_on(allowed).and_then(|p| p.ipv4)
                } else {
                    None
                };
                if let Some(dest) = dest {
                    node_dests.push((dest, via));
                }
            }
        }
        let routes = match allocations.and_then(|a| a.node_ip(&key, local_node)) {
            Some(local) => router_netns_routes(&node_dests, &subnets, local),
            // No address for this node on the segment yet: keep the node
            // routes, skip the ones that need it.
            None => router_netns_routes(&node_dests, &[], Ipv4Addr::UNSPECIFIED),
        };
        out.attachments.push(ContainerAttachment {
            docker_network: key,
            ip,
            routes,
            ifname: Some(ROUTER_IFNAME.to_string()),
            default_route: false,
        });
        out.router = true;
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLUSTER: &str = include_str!("../../../examples/cluster.yaml");

    fn cfg() -> ClusterConfig {
        let yaml = CLUSTER.replace(
            "  vpn:\n    type: egress\n",
            "  vpn:\n    type: egress\n  ctn2:\n    type: container\n    container: { stack: rt, service: router, subnet: 100.64.0.0/10, allowed_networks: [lan] }\n",
        );
        ClusterConfig::parse(&yaml).expect("cluster parses")
    }

    fn book() -> Allocations {
        let key = container_docker_name("ctn2");
        let mut a = Allocations::new("10.246.0.0/16".parse().unwrap(), 24);
        a.allocate_network(&key, &[]).unwrap();
        a
    }

    fn id(stack: &str, service: &str) -> InstanceId {
        InstanceId::new(stack, service, 0)
    }

    #[test]
    fn a_client_waits_for_its_own_address_but_not_for_the_router() {
        let cfg = cfg();
        let mut a = book();
        let client = id("site", "web");
        let nets = vec!["ctn2".to_string()];
        assert!(plan(&client, &nets, &cfg, Some(&a), "n1").is_err());

        let key = container_docker_name("ctn2");
        let ip = a.allocate_ip(&key, &client).unwrap();
        let p = plan(&client, &nets, &cfg, Some(&a), "n1").unwrap();
        assert!(!p.router);
        assert_eq!(p.attachments[0].ip, ip);
        assert_eq!(p.attachments[0].docker_network, key);
        assert!(!p.attachments[0].default_route);
        // No router address yet: fail closed.
        assert!(p.attachments[0].routes[0].contains(&"prohibit".to_string()));

        let router_ip = a.allocate_ip(&key, &id("rt", "router")).unwrap();
        let p = plan(&client, &nets, &cfg, Some(&a), "n1").unwrap();
        assert!(p.attachments[0].routes[0].contains(&router_ip.to_string()));
        assert_eq!(p.attachments[0].ip, ip);
    }

    #[test]
    fn the_router_gets_forwarding_an_ifname_and_node_routes() {
        let cfg = cfg();
        let mut a = book();
        let key = container_docker_name("ctn2");
        let router = id("rt", "router");
        assert!(plan(&router, &[], &cfg, Some(&a), "n1").is_err());
        let ip = a.allocate_ip(&key, &router).unwrap();
        let mut node_ips = Vec::new();
        for n in cfg.nodes.keys() {
            node_ips.push(a.allocate_node_ip(&key, n).unwrap());
        }
        let p = plan(&router, &[], &cfg, Some(&a), "n1").unwrap();
        assert!(p.router);
        let att = &p.attachments[0];
        assert_eq!(att.ip, ip);
        assert_eq!(att.ifname.as_deref(), Some("ctn0"));
        assert!(!att.routes.is_empty());
        let n1_via = a.node_ip(&key, "n1").unwrap().to_string();
        assert!(att.routes.iter().any(|r| r.contains(&n1_via)));
        assert!(att
            .routes
            .iter()
            .all(|r| r[0] == "route" && r[1] == "replace"));
    }
}
