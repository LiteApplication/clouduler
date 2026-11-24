use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use crate::cluster::{managed_gateway, ClusterConfig, Network, NetworkKind, Node, NodeAddress};
use crate::error::{ConfigError, Result};
use crate::netalloc::check_interface_name;

/// Addresses already taken, so two nodes cannot claim the same one.
#[derive(Default)]
struct TakenAddresses {
    internal: BTreeSet<IpAddr>,
    managed: BTreeMap<String, BTreeSet<IpAddr>>,
}

impl ClusterConfig {
    pub(super) fn validate_nodes(&self) -> Result<()> {
        if self.nodes.is_empty() {
            return Err(ConfigError::invalid("nodes", "at least one is required"));
        }
        if self.managers().next().is_none() {
            return Err(ConfigError::invalid(
                "nodes",
                "at least one node must have role manager",
            ));
        }

        let Some((internal, _)) = self.internal_network() else {
            return Err(ConfigError::invalid(
                "networks",
                "exactly one internal network is required",
            ));
        };
        let internal = internal.clone();
        let mut taken = TakenAddresses::default();

        for (name, node) in &self.nodes {
            let at = format!("nodes.{name}");
            if !node.network.contains_key(&internal) {
                return Err(ConfigError::invalid(
                    format!("{at}.network"),
                    format!("every node must have an address on the internal network {internal}"),
                ));
            }
            for (net_name, addr) in &node.network {
                let at = format!("{at}.network.{net_name}");
                let Some(net) = self.networks.get(net_name) else {
                    return Err(ConfigError::invalid(
                        at,
                        format!("no network named {net_name}"),
                    ));
                };
                let ctx = AddressCtx {
                    at: &at,
                    node_name: name,
                    node,
                    internal: &internal,
                    net_name,
                    net,
                };
                check_node_address(&ctx, addr, &mut taken)?;
            }
            self.check_node_drives(&at, node)?;
        }

        Ok(())
    }

    fn check_node_drives(&self, at: &str, node: &Node) -> Result<()> {
        for (drive, path) in &node.drives {
            if !self.drives.contains_key(drive) {
                return Err(ConfigError::invalid(
                    format!("{at}.drives.{drive}"),
                    format!("no drive named {drive} is declared under drives"),
                ));
            }
            if !path.is_absolute() {
                return Err(ConfigError::invalid(
                    format!("{at}.drives.{drive}"),
                    "must be an absolute path",
                ));
            }
        }
        Ok(())
    }
}

/// Everything a per-address check needs to name the offender in its error.
struct AddressCtx<'a> {
    at: &'a str,
    node_name: &'a str,
    node: &'a Node,
    internal: &'a str,
    net_name: &'a str,
    net: &'a Network,
}

fn check_node_address(
    ctx: &AddressCtx<'_>,
    addr: &NodeAddress,
    taken: &mut TakenAddresses,
) -> Result<()> {
    let at = ctx.at;
    let net = ctx.net;
    match (net.kind, addr) {
        (NetworkKind::Internal | NetworkKind::Private, NodeAddress::Ip(ip)) => {
            check_ip_address(ctx, ip, taken)
        }
        (NetworkKind::Private, NodeAddress::None) => check_unaddressed_private(ctx),
        (NetworkKind::Internal, NodeAddress::None) => Err(ConfigError::invalid(
            at,
            "the internal network needs an IP address on every node",
        )),
        (NetworkKind::Internal | NetworkKind::Private, _) => Err(ConfigError::invalid(
            at,
            format!("a {} network takes a bare IP address", net.kind),
        )),
        (NetworkKind::Public, NodeAddress::Public(public)) => {
            if public.is_empty() {
                return Err(ConfigError::invalid(
                    at,
                    "set at least one of ipv4, ipv6 or domain",
                ));
            }
            Ok(())
        }
        (NetworkKind::Public, _) => Err(ConfigError::invalid(
            at,
            "a public network takes an ipv4, ipv6 and/or domain block",
        )),
        (NetworkKind::Egress | NetworkKind::Wireguard, NodeAddress::Interface(addr)) => {
            check_interface_address(at, addr)
        }
        (NetworkKind::Egress | NetworkKind::Wireguard, _) => Err(ConfigError::invalid(
            at,
            format!("a {} network takes a bare host interface name", net.kind),
        )),
        (NetworkKind::Container, _) => Err(ConfigError::invalid(
            at,
            "a container network takes no node address, list the node under container.hosts to route its traffic through the router",
        )),
        (NetworkKind::Nat, _) => Err(ConfigError::invalid(
            at,
            "a nat network takes no node address, every node implements it the same way",
        )),
    }
}

fn internal_is_ipv4(ctx: &AddressCtx<'_>) -> bool {
    matches!(
        ctx.node.network.get(ctx.internal).and_then(|a| a.as_ip()),
        Some(IpAddr::V4(_))
    )
}

fn check_ip_address(ctx: &AddressCtx<'_>, ip: &IpAddr, taken: &mut TakenAddresses) -> Result<()> {
    let (at, net, net_name) = (ctx.at, ctx.net, ctx.net_name);
    let Some(subnet) = net.subnet else {
        return Err(ConfigError::invalid(
            at,
            format!("network {net_name} has no subnet"),
        ));
    };
    if !subnet.contains(ip) {
        return Err(ConfigError::invalid(
            at,
            format!("{ip} is outside the {net_name} subnet {subnet}"),
        ));
    }
    if net_name == ctx.internal && !taken.internal.insert(*ip) {
        return Err(ConfigError::invalid(
            at,
            format!("{ip} is used by another node"),
        ));
    }
    if let Some(v4) = net.managed_subnet() {
        let IpAddr::V4(ip4) = ip else {
            return Err(ConfigError::invalid(
                at,
                "a managed network takes an IPv4 address",
            ));
        };
        // The tunnel to the other nodes and the forwarding NAT
        // run on the internal address, which must be IPv4.
        if !internal_is_ipv4(ctx) {
            return Err(ConfigError::invalid(
                at,
                format!(
                    "node {} has no IPv4 address on the internal network {}, which a managed network needs for the VXLAN underlay; give it one or remove its address on {net_name}",
                    ctx.node_name, ctx.internal
                ),
            ));
        }
        if *ip4 == v4.network() || *ip4 == v4.broadcast() {
            return Err(ConfigError::invalid(
                at,
                format!("{ip} is the network or broadcast address of {v4}"),
            ));
        }
        if *ip4 == managed_gateway(v4) {
            return Err(ConfigError::invalid(
                at,
                format!(
                    "{ip} is the gateway of the managed network {net_name}, pick another address"
                ),
            ));
        }
        if !taken
            .managed
            .entry(net_name.to_string())
            .or_default()
            .insert(*ip)
        {
            return Err(ConfigError::invalid(
                at,
                format!("{ip} is used by another node on {net_name}"),
            ));
        }
    }
    Ok(())
}

/// A private network listed without an address (managed ones use this to
/// give a node access without a node address).
fn check_unaddressed_private(ctx: &AddressCtx<'_>) -> Result<()> {
    let (at, net, net_name) = (ctx.at, ctx.net, ctx.net_name);
    if net.subnet.is_none() {
        return Err(ConfigError::invalid(
            at,
            format!("network {net_name} has no subnet"),
        ));
    }
    if net.is_managed() && !internal_is_ipv4(ctx) {
        return Err(ConfigError::invalid(
            at,
            format!(
                "node {} has no IPv4 address on the internal network {}, which a managed network needs for the VXLAN underlay; give it one or remove {net_name} from the node",
                ctx.node_name, ctx.internal
            ),
        ));
    }
    Ok(())
}

fn check_interface_address(at: &str, addr: &crate::cluster::InterfaceAddress) -> Result<()> {
    if addr.interface.trim().is_empty() {
        return Err(ConfigError::invalid(
            at,
            "must name a non-empty host interface",
        ));
    }
    if let Err(why) = check_interface_name(&addr.interface) {
        return Err(ConfigError::invalid(at, why));
    }
    if let Some(fallback) = &addr.fallback {
        let fb = format!("{at}.fallback");
        if fallback.interface.trim().is_empty() {
            return Err(ConfigError::invalid(
                fb,
                "must name a non-empty host interface",
            ));
        }
        if let Err(why) = check_interface_name(&fallback.interface) {
            return Err(ConfigError::invalid(fb, why));
        }
        if fallback.interface == addr.interface {
            return Err(ConfigError::invalid(
                fb,
                "must name a different interface than the primary one",
            ));
        }
    }
    Ok(())
}
