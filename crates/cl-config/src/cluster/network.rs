use super::*;
use crate::stack::Protocol;
use ipnet::{IpNet, Ipv4Net};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum NetworkKind {
    /// The one network every node talks to every other node on. Carries raft,
    /// drive sync and the container overlay. Trusted, so unencrypted.
    Internal,
    /// A further non public subnet a node may sit on. Records for its domain
    /// resolve to the node address on that network.
    Private,
    /// Reachable from the internet. Records resolve to the node public address.
    Public,
    /// Not an overlay: a node's address on this network names a local host
    /// interface (a VPN tunnel, usually) that a service attached to it
    /// egresses through, instead of the node's normal route out.
    Egress,
    /// Like `Egress`, but the tunnel is a WireGuard interface clouduler brings up
    /// from an imported config. The node address is the derived interface name.
    Wireguard,
    /// Like `Egress`, but outbound traffic leaves through the node's normal route
    /// (masqueraded). No node address is declared. For services that must reach
    /// addresses only the host routing table knows (a VPN peer, say).
    Nat,
    /// Traffic to the network's `subnet` is routed through a router container.
    /// No node address is declared; the subnet is the routed range.
    Container,
}

impl NetworkKind {
    /// Internal and private networks address nodes by IP inside a subnet.
    pub fn is_subnetted(self) -> bool {
        matches!(self, NetworkKind::Internal | NetworkKind::Private)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct Network {
    #[serde(rename = "type")]
    pub kind: NetworkKind,
    /// Required for internal and private networks, where it must contain the
    /// node addresses, and for a container network, where it is the address
    /// space routed through the router (IPv4 only, `0.0.0.0/0` allowed).
    #[cfg_attr(feature = "ts", ts(optional, as = "Option<String>"))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subnet: Option<IpNet>,
    /// Records under this domain resolve to node addresses on this network.
    /// On a container network it only qualifies the names the client DNS
    /// answers for the router side; nothing is published to a DNS provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Whether hostnames on this network are published to the DNS provider
    /// (Cloudflare). `false` keeps a private network's names out of public
    /// DNS; they are then answered only by `proxy.dns_server`. Always true
    /// for a public network.
    #[serde(default = "default_true_bool", skip_serializing_if = "is_true")]
    pub publish_dns: bool,
    /// Required for, and only allowed on, a `container` network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerNetwork>,
    /// Networks this one may reach (internal, private and container networks).
    /// Absent means no host firewall (unrestricted at L3) and only own network plus
    /// public networks at L7; `[]` or a list turns enforcement on. See
    /// [`ClusterConfig::reaches`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<Vec<String>>,
    /// A cluster managed private network: clouduler builds one cluster-wide
    /// VXLAN overlay over `subnet` between the nodes that list an address on
    /// it, and allocates container addresses from it. Only valid on a
    /// `private` network with an IPv4 subnet.
    #[serde(default, skip_serializing_if = "is_false")]
    pub managed: bool,
    /// Managed networks only: the allowlist of ports reachable from outside on
    /// a node's address, forwarded to the named service's container. Exposed
    /// only on the node holding the container.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_ports: Vec<ManagedOpenPort>,
    /// Managed networks only: the reverse proxy serves 80/443 for this
    /// network's hostnames on any node, and their DNS is published for the
    /// network instead of following `proxy.dns_publish`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub proxy: bool,
    /// Internal and private networks only: per link (this network as the
    /// client side, the key as the target network a route lives on), whether
    /// the client DNS server (`proxy.dns_server`) answers clients here with the
    /// node's address on this network (the shortcut) instead of the route's own.
    /// A missing key means `true`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub shortcut: BTreeMap<String, bool>,
}

/// One port a managed network exposes on the address of the node holding the
/// container of `stack`/`service`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct ManagedOpenPort {
    /// Port on the node address.
    pub port: u16,
    #[serde(default)]
    pub protocol: Protocol,
    /// Port in the container; defaults to `port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_port: Option<u16>,
    /// Stack holding the service.
    pub stack: String,
    /// Service whose container receives the traffic.
    pub service: String,
}

impl ManagedOpenPort {
    pub fn effective_target_port(&self) -> u16 {
        self.target_port.unwrap_or(self.port)
    }
}

/// A `container` network: the router and what its side may reach. The routed
/// range is the network's own `subnet`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct ContainerNetwork {
    /// Stack holding the router service.
    pub stack: String,
    /// Router service; it must run a single replica.
    pub service: String,
    /// Nodes whose host-level traffic to `subnet` goes through the router.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// Host ports the router side may reach.
    #[serde(default)]
    pub open_ports: Vec<ContainerOpenPort>,
    /// Let the router side reach the reverse proxy on 80/443.
    #[serde(default, skip_serializing_if = "is_false")]
    pub proxy: bool,
}

/// One host port opened to the router side of a container network.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct ContainerOpenPort {
    pub port: u16,
    #[serde(default)]
    pub protocol: Protocol,
    /// Port on the host the traffic lands on; defaults to `port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_port: Option<u16>,
}

/// A container network's block together with its routed range, which lives
/// on the owning [`Network`]. Derefs to the block.
#[derive(Debug, Clone, Copy)]
pub struct ContainerView<'a> {
    container: &'a ContainerNetwork,
    /// Address space behind the router, `0.0.0.0/0` allowed.
    pub subnet: Ipv4Net,
}

impl std::ops::Deref for ContainerView<'_> {
    type Target = ContainerNetwork;
    fn deref(&self) -> &ContainerNetwork {
        self.container
    }
}

/// Smallest and largest prefix a managed network's subnet may have.
pub const MANAGED_MIN_PREFIX: u8 = 16;

pub const MANAGED_MAX_PREFIX: u8 = 29;

/// The anycast gateway of a managed subnet: its first host address.
pub fn managed_gateway(subnet: Ipv4Net) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(subnet.network()).saturating_add(1))
}

impl Network {
    /// Whether clouduler manages this network's overlay (a validated managed
    /// network is always private with an IPv4 subnet).
    pub fn is_managed(&self) -> bool {
        self.managed && self.kind == NetworkKind::Private
    }

    /// The IPv4 subnet of a managed network.
    pub fn managed_subnet(&self) -> Option<Ipv4Net> {
        match (self.is_managed(), self.subnet) {
            (true, Some(IpNet::V4(v4))) => Some(v4),
            _ => None,
        }
    }

    /// The address space a container network routes through its router, when
    /// this is a container network with an IPv4 subnet.
    pub fn routed_subnet(&self) -> Option<Ipv4Net> {
        match (self.kind, self.subnet) {
            (NetworkKind::Container, Some(IpNet::V4(v4))) => Some(v4),
            _ => None,
        }
    }
}

/// Stable position (0-based, counted down from the last host address) where
/// the automatic node address of `node` starts probing.
pub(super) fn auto_slot(node: &str, hosts: u64) -> u64 {
    crate::netalloc::fnv1a(node.as_bytes()) % hosts.max(1)
}

impl ContainerOpenPort {
    pub fn effective_target_port(&self) -> u16 {
        self.target_port.unwrap_or(self.port)
    }
}

impl ClusterConfig {
    /// The network `ip` is a client of: the subnetted network whose subnet
    /// holds it, else the first public network for a globally routable
    /// address. `None` for anything else (loopback, an unknown private range).
    pub fn client_network(&self, ip: std::net::IpAddr) -> Option<&str> {
        let subnetted = self
            .networks
            .iter()
            .find(|(name, net)| net.kind.is_subnetted() && self.ip_in_network(name, ip));
        subnetted
            .or_else(|| {
                self.networks.iter().find(|(name, net)| {
                    net.kind == NetworkKind::Public && self.ip_in_network(name, ip)
                })
            })
            .map(|(name, _)| name.as_str())
            .or_else(|| self.container_client_network(ip))
    }

    /// The container network whose routed range holds `ip`, for one that
    /// lets its router side reach the reverse proxy: those peers are served
    /// like a private network's clients. A default-route network
    /// (`0.0.0.0/0`) claims no client, since it would claim everyone.
    fn container_client_network(&self, ip: std::net::IpAddr) -> Option<&str> {
        let std::net::IpAddr::V4(ip) = ip.to_canonical() else {
            return None;
        };
        self.container_networks()
            .find(|(_, c)| c.proxy && c.subnet.prefix_len() > 0 && c.subnet.contains(&ip))
            .map(|(name, _)| name.as_str())
    }

    /// The container networks the client DNS server answers for, and so
    /// listens for on each node's segment address: those that let the router
    /// side reach the proxy, when both the proxy and the DNS server are on
    /// (and, if `proxy.dns_server.networks` is set, listed there).
    pub fn container_dns_networks(&self) -> impl Iterator<Item = (&String, ContainerView<'_>)> {
        let dns = &self.proxy.dns_server;
        let on = self.proxy.enabled && dns.enabled;
        self.container_networks().filter(move |(name, c)| {
            on && c.proxy && (dns.networks.is_empty() || dns.networks.contains(name))
        })
    }

    /// Is `ip` an address of `network`'s own clients? A subnetted network
    /// owns the addresses inside its subnet; a public network owns every
    /// globally routable address, i.e. one that is not private, loopback,
    /// link-local, CGNAT or inside any configured subnet. Other kinds have no
    /// clients of their own.
    pub fn ip_in_network(&self, network: &str, ip: std::net::IpAddr) -> bool {
        let ip = ip.to_canonical();
        let Some(net) = self.networks.get(network) else {
            return false;
        };
        match net.kind {
            NetworkKind::Internal | NetworkKind::Private => {
                net.subnet.is_some_and(|s| s.contains(&ip))
            }
            NetworkKind::Public => {
                !self
                    .networks
                    .values()
                    .filter(|n| n.kind.is_subnetted())
                    .filter_map(|n| n.subnet)
                    .any(|s| s.contains(&ip))
                    && is_globally_routable(ip)
            }
            _ => false,
        }
    }
}

/// Append `domain` to a bare label. A name containing a dot is taken as full.
/// `@` names the apex (the domain alone).
///
/// A trailing dot is stripped and the name used verbatim regardless of
/// `domain`, to name a domain outside any network here. Callers needing to
/// detect that (ACME HTTP-01 fallback) check `name.ends_with('.')` on the raw
/// input; this only normalizes.
pub fn qualify(name: &str, domain: Option<&str>) -> String {
    // DNS names are case-insensitive and resolvers/Cloudflare hand them back
    // lowercased, so normalize here to keep every comparison downstream exact.
    let qualified = if let Some(stripped) = name.strip_suffix('.') {
        stripped.to_string()
    } else {
        match domain {
            Some(domain) if name == "@" => domain.to_string(),
            Some(domain) if !name.contains('.') => format!("{name}.{domain}"),
            _ => name.to_string(),
        }
    };
    qualified.to_ascii_lowercase()
}

impl fmt::Display for NetworkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetworkKind::Internal => f.write_str("internal"),
            NetworkKind::Private => f.write_str("private"),
            NetworkKind::Public => f.write_str("public"),
            NetworkKind::Egress => f.write_str("egress"),
            NetworkKind::Wireguard => f.write_str("wireguard"),
            NetworkKind::Nat => f.write_str("nat"),
            NetworkKind::Container => f.write_str("container"),
        }
    }
}

impl ClusterConfig {
    /// The `access` list of network `name`; empty when absent or unknown.
    pub fn access_of(&self, name: &str) -> &[String] {
        self.networks
            .get(name)
            .and_then(|n| n.access.as_deref())
            .unwrap_or(&[])
    }

    /// Whether traffic from network `from` may reach network `to`: always for
    /// itself, always for a public `to`, otherwise when `from`'s `access`
    /// lists `to`. An absent `access` counts as empty here.
    pub fn reaches(&self, from: &str, to: &str) -> bool {
        from == to
            || self
                .networks
                .get(to)
                .is_some_and(|n| n.kind == NetworkKind::Public)
            || self.access_of(from).iter().any(|n| n == to)
    }

    /// Names of every `Egress`, `Wireguard` or `Nat` network, the kinds a
    /// service's `networks:` entry can join for outbound NAT.
    pub fn egress_networks(&self) -> std::collections::BTreeSet<String> {
        self.networks
            .iter()
            .filter(|(_, net)| {
                matches!(
                    net.kind,
                    NetworkKind::Egress | NetworkKind::Wireguard | NetworkKind::Nat
                )
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Every `container` network with its config and routed range. One whose
    /// block or IPv4 subnet is missing is skipped; validation rejects it.
    pub fn container_networks(&self) -> impl Iterator<Item = (&String, ContainerView<'_>)> {
        self.networks.iter().filter_map(|(name, net)| {
            let container = net.container.as_ref()?;
            let subnet = net.routed_subnet()?;
            Some((name, ContainerView { container, subnet }))
        })
    }

    /// The container network whose router is `stack`/`service`, if any.
    pub fn container_router_of(&self, stack: &str, service: &str) -> Option<&str> {
        self.container_networks()
            .find(|(_, c)| c.stack == stack && c.service == service)
            .map(|(name, _)| name.as_str())
    }

    /// Names of every network all nodes implement without per-node setup (`Nat`
    /// and `Container`); they take no entry under `nodes.<name>.network`.
    /// Placement treats membership as an automatic pass in
    /// `cl_sched::place::rejection`, like `served_drives`.
    pub fn everywhere_networks(&self) -> std::collections::BTreeSet<String> {
        self.networks
            .iter()
            .filter(|(_, net)| matches!(net.kind, NetworkKind::Nat | NetworkKind::Container))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Every managed network with its IPv4 subnet, in name order.
    pub fn managed_networks(&self) -> Vec<(&str, &Network, Ipv4Net)> {
        self.networks
            .iter()
            .filter_map(|(name, net)| Some((name.as_str(), net, net.managed_subnet()?)))
            .collect()
    }

    /// The address of every node implementing managed network `network`
    /// (listing it, with or without an address), by node name. An explicit
    /// address is taken as is; `null` gets an automatic one from the top of
    /// the subnet downward (the container pool fills from the bottom), picked
    /// by a stable hash of the node name and probed past the gateway,
    /// explicit addresses and earlier nodes' addresses, so adding or removing
    /// another node moves an address only on a hash collision. Empty when
    /// `network` is not managed; a node the subnet has no room for is absent.
    pub fn managed_node_addresses(&self, network: &str) -> BTreeMap<&str, Ipv4Addr> {
        let mut out = BTreeMap::new();
        let Some(subnet) = self.networks.get(network).and_then(Network::managed_subnet) else {
            return out;
        };
        let gateway = managed_gateway(subnet);
        let first = u32::from(subnet.network());
        let last = u32::from(subnet.broadcast());
        let mut taken: BTreeSet<Ipv4Addr> = BTreeSet::new();
        taken.insert(subnet.network());
        taken.insert(subnet.broadcast());
        taken.insert(gateway);
        let mut auto = Vec::new();
        for (name, node) in &self.nodes {
            match node.network.get(network) {
                Some(NodeAddress::Ip(IpAddr::V4(ip))) => {
                    taken.insert(*ip);
                    out.insert(name.as_str(), *ip);
                }
                Some(NodeAddress::None) => auto.push(name.as_str()),
                _ => {}
            }
        }
        // Host addresses are first+1 ..= last-1; slot 0 is last-1.
        let hosts = u64::from(last - first).saturating_sub(1);
        for name in auto {
            let start = auto_slot(name, hosts);
            for i in 0..hosts {
                let slot = (start + i) % hosts;
                let ip = Ipv4Addr::from(last - 1 - slot as u32);
                if taken.insert(ip) {
                    out.insert(name, ip);
                    break;
                }
            }
        }
        out
    }

    /// The address `node` holds on managed network `network`: its explicit
    /// one, or the automatic one (see [`Self::managed_node_addresses`]).
    /// `None` when the network is not managed or the node does not list it.
    pub fn node_address_on(&self, node: &str, network: &str) -> Option<Ipv4Addr> {
        self.managed_node_addresses(network).get(node).copied()
    }

    /// The ports managed network `network` exposes (empty when it is not
    /// managed or opens none).
    pub fn managed_open_ports(&self, network: &str) -> &[ManagedOpenPort] {
        match self.networks.get(network) {
            Some(n) if n.is_managed() => &n.open_ports,
            _ => &[],
        }
    }

    /// Whether the reverse proxy serves managed network `network` on any
    /// node (`proxy: true`).
    pub fn managed_proxy(&self, network: &str) -> bool {
        self.networks
            .get(network)
            .is_some_and(|n| n.is_managed() && n.proxy)
    }

    /// Whether `network` is a managed network.
    pub fn is_managed_network(&self, network: &str) -> bool {
        self.networks.get(network).is_some_and(Network::is_managed)
    }

    /// The addresses of managed network `network` no instance may take: the
    /// network and broadcast addresses, the gateway and every node address.
    pub fn managed_reserved(&self, network: &str) -> BTreeSet<Ipv4Addr> {
        let mut out = BTreeSet::new();
        let Some(subnet) = self.networks.get(network).and_then(Network::managed_subnet) else {
            return out;
        };
        out.insert(subnet.network());
        out.insert(subnet.broadcast());
        out.insert(managed_gateway(subnet));
        out.extend(self.managed_node_addresses(network).into_values());
        out
    }

    /// Names of the nodes holding an IP address on `network`. On a managed
    /// network only these can serve the login portal and the dashboard, since
    /// a node's address there is the one a client is pointed to.
    pub fn addressed_nodes_on(&self, network: &str) -> Vec<&String> {
        if self.is_managed_network(network) {
            // Every implementing node holds an address (explicit or automatic).
            return self
                .nodes
                .iter()
                .filter(|(_, node)| node.network.contains_key(network))
                .map(|(name, _)| name)
                .collect();
        }
        self.nodes
            .iter()
            .filter(|(_, node)| node.ip_on(network).is_some())
            .map(|(name, _)| name)
            .collect()
    }

    /// Names of every public or wireguard network, and of every unmanaged
    /// private network on which some node declares `open_ports`: their
    /// reachability depends on a specific node, so a raw `x-clouduler.expose`
    /// port only schedules onto a node that has it open. A private network
    /// where no node lists a port stays ungated, as it always was.
    pub fn port_gated_networks(&self) -> std::collections::BTreeSet<String> {
        self.networks
            .iter()
            .filter(|(name, net)| {
                matches!(net.kind, NetworkKind::Public | NetworkKind::Wireguard)
                    || (net.kind == NetworkKind::Private
                        && !net.is_managed()
                        && self.nodes.values().any(|node| {
                            node.network.get(name.as_str()).is_some_and(|addr| {
                                addr.opens_all_ports()
                                    || addr.open_ports().is_some_and(|ports| !ports.is_empty())
                            })
                        }))
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The single internal network; validation rejects a config without one.
    pub fn internal_network(&self) -> Option<(&String, &Network)> {
        self.networks
            .iter()
            .find(|(_, n)| n.kind == NetworkKind::Internal)
    }
}
