use std::collections::{BTreeMap, BTreeSet};

use crate::cluster::{
    ClusterConfig, ContainerNetwork, Network, NetworkKind, MANAGED_MAX_PREFIX, MANAGED_MIN_PREFIX,
};
use crate::error::{ConfigError, Result};
use crate::netalloc::{
    container_mark, container_table_ids, egress_table_id, subnet_octet, EGRESS_POOL_BASE,
    NAT_POOL_BASE,
};

impl ClusterConfig {
    pub(super) fn validate_networks(&self) -> Result<()> {
        let mut internal = Vec::new();
        let mut domains = BTreeSet::new();

        for (name, net) in &self.networks {
            if net.kind == NetworkKind::Internal {
                internal.push(name.as_str());
            }
            check_subnet_presence(name, net)?;
            self.check_managed_or_plain(name, net)?;
            if let Some(access) = &net.access {
                self.validate_access(name, net.kind, access)?;
            }
            check_publish_dns(name, net)?;
            check_unaddressable_has_no_domain(name, net)?;
            self.check_container_block(name, net)?;
            check_domain(name, net, &mut domains)?;
        }

        self.validate_derived_allocations()?;
        check_single_internal(&internal)
    }

    fn check_managed_or_plain(&self, name: &str, net: &Network) -> Result<()> {
        if net.managed {
            check_managed_network(self, name, net)
        } else if !net.open_ports.is_empty() || net.proxy {
            let field = if net.proxy { "proxy" } else { "open_ports" };
            Err(ConfigError::invalid(
                format!("networks.{name}.{field}"),
                format!("{field} is only allowed on a managed network (managed: true on a private network)"),
            ))
        } else {
            Ok(())
        }
    }

    fn check_container_block(&self, name: &str, net: &Network) -> Result<()> {
        let at = format!("networks.{name}");
        match (net.kind, &net.container) {
            (NetworkKind::Container, None) => Err(ConfigError::invalid(
                format!("{at}.container"),
                "a container network needs a container block (stack, service) and a subnet",
            )),
            (NetworkKind::Container, Some(c)) => {
                let Some(routed) = net.routed_subnet() else {
                    return Err(ConfigError::invalid(
                        format!("{at}.subnet"),
                        "a container network needs an IPv4 subnet, the address space routed through its router",
                    ));
                };
                self.validate_container_network(name, c, routed)
            }
            (_, Some(_)) => Err(ConfigError::invalid(
                format!("{at}.container"),
                format!("a {} network must not declare a container block", net.kind),
            )),
            _ => Ok(()),
        }
    }

    /// `access` is the source side of the network graph: only networks that
    /// have clients can reach anything, and every target must be a network
    /// that traffic can actually be addressed to.
    fn validate_access(&self, name: &str, kind: NetworkKind, access: &[String]) -> Result<()> {
        let at = format!("networks.{name}.access");
        if !matches!(
            kind,
            NetworkKind::Internal | NetworkKind::Private | NetworkKind::Container
        ) {
            return Err(ConfigError::invalid(
                at,
                format!(
                    "a {kind} network has no clients of its own; access is only allowed on internal, private and container networks"
                ),
            ));
        }
        let mut seen = BTreeSet::new();
        for target in access {
            let Some(t) = self.networks.get(target) else {
                return Err(ConfigError::invalid(
                    at,
                    format!("no network named {target}"),
                ));
            };
            if target == name {
                return Err(ConfigError::invalid(
                    at,
                    "a network always reaches itself; remove it from its own access list",
                ));
            }
            if !matches!(
                t.kind,
                NetworkKind::Internal
                    | NetworkKind::Private
                    | NetworkKind::Public
                    | NetworkKind::Container
            ) {
                return Err(ConfigError::invalid(
                    at,
                    format!(
                        "{target} is a {} network, only internal, private, public and container networks can be reached",
                        t.kind
                    ),
                ));
            }
            if !seen.insert(target) {
                return Err(ConfigError::invalid(
                    at,
                    format!("{target} is listed more than once"),
                ));
            }
        }
        Ok(())
    }

    fn validate_container_network(
        &self,
        name: &str,
        c: &ContainerNetwork,
        routed: ipnet::Ipv4Net,
    ) -> Result<()> {
        let at = format!("networks.{name}.container");
        let bad = |field: &str, msg: String| ConfigError::invalid(format!("{at}.{field}"), msg);
        let bad_subnet = |msg: String| ConfigError::invalid(format!("networks.{name}.subnet"), msg);

        if c.stack.trim().is_empty() {
            return Err(bad("stack", "must name the router stack".into()));
        }
        if c.service.trim().is_empty() {
            return Err(bad("service", "must name the router service".into()));
        }

        let mut seen = BTreeSet::new();
        for host in &c.hosts {
            if !self.nodes.contains_key(host) {
                return Err(bad("hosts", format!("no node named {host}")));
            }
            if !seen.insert(host) {
                return Err(bad("hosts", format!("{host} is listed more than once")));
            }
        }

        let mut seen = BTreeSet::new();
        for port in &c.open_ports {
            if !seen.insert((port.port, port.protocol)) {
                return Err(bad(
                    "open_ports",
                    format!("{}/{} is listed more than once", port.port, port.protocol),
                ));
            }
            if port.port == 0 || port.target_port == Some(0) {
                return Err(bad("open_ports", "port 0 is not valid".into()));
            }
        }

        // A route for `subnet` that sits inside a range the underlay uses would
        // hijack it. A subnet that contains those ranges (0.0.0.0/0) is fine:
        // the more specific routes win.
        let inside = |other: ipnet::Ipv4Net, what: &str| -> Result<()> {
            if other.contains(&routed) {
                return Err(bad_subnet(format!("{routed} lies inside {what} {other}")));
            }
            Ok(())
        };
        for (other_name, other) in &self.networks {
            if let (true, Some(ipnet::IpNet::V4(v4))) = (other.kind.is_subnetted(), other.subnet) {
                inside(v4, &format!("the subnet of network {other_name}"))?;
            }
        }
        for (octet, what) in [
            (244u8, "the overlay pool"),
            (EGRESS_POOL_BASE, "the egress pool"),
            (NAT_POOL_BASE, "the nat pool"),
        ] {
            inside(
                ipnet::Ipv4Net::new_assert(std::net::Ipv4Addr::new(10, octet, 0, 0), 16),
                what,
            )?;
        }
        Ok(())
    }

    /// Egress, wireguard and nat networks derive their bridge subnet and (for the
    /// first two) routing table from a hash of their name. Names that hash alike
    /// would share them and rewrite each other's default route every tick, so
    /// refuse the config; the allocation cannot change without renumbering live
    /// networks.
    fn validate_derived_allocations(&self) -> Result<()> {
        let mut egress_octets: BTreeMap<u8, &str> = BTreeMap::new();
        let mut nat_octets: BTreeMap<u8, &str> = BTreeMap::new();
        let mut tables: BTreeMap<u32, &str> = BTreeMap::new();
        let mut marks: BTreeMap<u32, &str> = BTreeMap::new();

        for (name, net) in &self.networks {
            let at = format!("networks.{name}");
            match net.kind {
                NetworkKind::Egress | NetworkKind::Wireguard => {
                    let octet = subnet_octet(name);
                    if let Some(other) = egress_octets.insert(octet, name) {
                        return Err(ConfigError::invalid(
                            at,
                            format!(
                                "networks {other} and {name} both derive the bridge subnet 10.{EGRESS_POOL_BASE}.{octet}.0/24 \
                                 from their names; rename one of them"
                            ),
                        ));
                    }
                    let table = egress_table_id(name);
                    if let Some(other) = tables.insert(table, name) {
                        return Err(ConfigError::invalid(
                            at,
                            format!(
                                "networks {other} and {name} both derive the policy routing table {table} \
                                 from their names; rename one of them"
                            ),
                        ));
                    }
                }
                NetworkKind::Container => {
                    let mark = container_mark(name);
                    if let Some(other) = marks.insert(mark, name) {
                        return Err(ConfigError::invalid(
                            at,
                            format!(
                                "networks {other} and {name} both derive the packet mark {mark:#x} \
                                 from their names; rename one of them"
                            ),
                        ));
                    }
                    let (reply, dst) = container_table_ids(name);
                    for table in [reply, dst] {
                        if let Some(other) = tables.insert(table, name) {
                            return Err(ConfigError::invalid(
                                at,
                                format!(
                                    "networks {other} and {name} both derive the policy routing table {table} \
                                     from their names; rename one of them"
                                ),
                            ));
                        }
                    }
                }
                NetworkKind::Nat => {
                    let octet = subnet_octet(name);
                    if let Some(other) = nat_octets.insert(octet, name) {
                        return Err(ConfigError::invalid(
                            at,
                            format!(
                                "networks {other} and {name} both derive the bridge subnet 10.{NAT_POOL_BASE}.{octet}.0/24 \
                                 from their names; rename one of them"
                            ),
                        ));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn check_subnet_presence(name: &str, net: &Network) -> Result<()> {
    let at = format!("networks.{name}");
    match (net.kind.is_subnetted(), net.subnet) {
        (true, None) => Err(ConfigError::invalid(
            at,
            format!("a {} network needs a subnet", net.kind),
        )),
        (false, Some(_)) if net.kind != NetworkKind::Container => Err(ConfigError::invalid(
            at,
            format!("a {} network must not declare a subnet", net.kind),
        )),
        _ => Ok(()),
    }
}

fn check_publish_dns(name: &str, net: &Network) -> Result<()> {
    if net.kind == NetworkKind::Public && !net.publish_dns {
        return Err(ConfigError::invalid(
            format!("networks.{name}"),
            "a public network is always published to dns; publish_dns can only be turned off on internal and private networks",
        ));
    }
    Ok(())
}

fn check_unaddressable_has_no_domain(name: &str, net: &Network) -> Result<()> {
    if matches!(
        net.kind,
        NetworkKind::Egress | NetworkKind::Wireguard | NetworkKind::Nat
    ) && net.domain.is_some()
    {
        return Err(ConfigError::invalid(
            format!("networks.{name}.domain"),
            format!(
                "a {} network is not addressable and must not declare a domain",
                net.kind
            ),
        ));
    }
    Ok(())
}

fn check_domain(name: &str, net: &Network, domains: &mut BTreeSet<String>) -> Result<()> {
    let Some(domain) = &net.domain else {
        return Ok(());
    };
    let at = format!("networks.{name}");
    if domain.trim().is_empty() || domain.starts_with('.') || domain.ends_with('.') {
        return Err(ConfigError::invalid(
            format!("{at}.domain"),
            "must be a bare domain name",
        ));
    }
    // Two networks owning one domain makes record ownership ambiguous.
    if !domains.insert(domain.to_ascii_lowercase()) {
        return Err(ConfigError::invalid(
            format!("{at}.domain"),
            format!("{domain} is already claimed by another network"),
        ));
    }
    Ok(())
}

fn check_single_internal(internal: &[&str]) -> Result<()> {
    match internal.len() {
        1 => Ok(()),
        0 => Err(ConfigError::invalid(
            "networks",
            "exactly one internal network is required for node to node traffic",
        )),
        _ => Err(ConfigError::invalid(
            "networks",
            format!(
                "only one internal network is allowed, found {}",
                internal.join(", ")
            ),
        )),
    }
}

/// A managed network is a private network with an IPv4 subnet of /16../29
/// that overlaps no other cluster subnet and none of the pools clouduler
/// derives overlays and bridges from.
fn check_managed_network(cfg: &ClusterConfig, name: &str, net: &Network) -> Result<()> {
    let at = format!("networks.{name}");
    let subnet = check_managed_subnet_shape(&at, net)?;
    check_managed_pools(&at, subnet)?;
    check_managed_open_ports(&at, net)?;
    check_managed_capacity(cfg, name, &at, subnet)?;
    check_managed_overlaps(cfg, name, &at, subnet)
}

fn overlaps(a: ipnet::Ipv4Net, b: ipnet::Ipv4Net) -> bool {
    a.contains(&b.network()) || b.contains(&a.network())
}

fn check_managed_subnet_shape(at: &str, net: &Network) -> Result<ipnet::Ipv4Net> {
    if net.kind != NetworkKind::Private {
        return Err(ConfigError::invalid(
            format!("{at}.managed"),
            format!(
                "only a private network can be managed, this one is {}",
                net.kind
            ),
        ));
    }
    let Some(subnet) = net.managed_subnet() else {
        return Err(ConfigError::invalid(
            format!("{at}.subnet"),
            "a managed network needs an IPv4 subnet",
        ));
    };
    if !(MANAGED_MIN_PREFIX..=MANAGED_MAX_PREFIX).contains(&subnet.prefix_len()) {
        return Err(ConfigError::invalid(
            format!("{at}.subnet"),
            format!(
                "{subnet}: the prefix length of a managed network must be between /{MANAGED_MIN_PREFIX} and /{MANAGED_MAX_PREFIX}"
            ),
        ));
    }
    if subnet.network() != subnet.addr() {
        return Err(ConfigError::invalid(
            format!("{at}.subnet"),
            format!(
                "{subnet} has host bits set, did you mean {}?",
                subnet.trunc()
            ),
        ));
    }
    Ok(subnet)
}

fn check_managed_pools(at: &str, subnet: ipnet::Ipv4Net) -> Result<()> {
    for (octet, what) in [
        (244u8, "the overlay pool"),
        (EGRESS_POOL_BASE, "the egress pool"),
        (NAT_POOL_BASE, "the nat pool"),
    ] {
        let pool = ipnet::Ipv4Net::new_assert(std::net::Ipv4Addr::new(10, octet, 0, 0), 16);
        if overlaps(subnet, pool) {
            return Err(ConfigError::invalid(
                format!("{at}.subnet"),
                format!("{subnet} overlaps {what} {pool}"),
            ));
        }
    }
    Ok(())
}

fn check_managed_open_ports(at: &str, net: &Network) -> Result<()> {
    let bad = |msg: String| ConfigError::invalid(format!("{at}.open_ports"), msg);
    let mut seen = BTreeSet::new();
    for p in &net.open_ports {
        if p.port == 0 || p.target_port == Some(0) {
            return Err(bad("port 0 is not valid".into()));
        }
        if !seen.insert((p.port, p.protocol)) {
            return Err(bad(format!(
                "{}/{} is listed more than once",
                p.port, p.protocol
            )));
        }
        if p.stack.trim().is_empty() || p.service.trim().is_empty() {
            return Err(bad(format!(
                "{}/{} must name the stack and service it maps to",
                p.port, p.protocol
            )));
        }
        if net.proxy && p.protocol == crate::stack::Protocol::Tcp && matches!(p.port, 80 | 443) {
            return Err(bad(format!(
                "{}/tcp is served by the reverse proxy (proxy: true) and cannot also be an open port",
                p.port
            )));
        }
    }
    Ok(())
}

/// Every implementing node needs room for an address next to the gateway, and
/// the pool must keep some room for containers.
fn check_managed_capacity(
    cfg: &ClusterConfig,
    name: &str,
    at: &str,
    subnet: ipnet::Ipv4Net,
) -> Result<()> {
    let implementing = cfg
        .nodes
        .values()
        .filter(|n| n.network.contains_key(name))
        .count() as u64;
    let hosts = (1u64 << (32 - subnet.prefix_len())) - 2;
    if implementing + 1 >= hosts {
        return Err(ConfigError::invalid(
            format!("{at}.subnet"),
            format!("{subnet} is too small for the gateway, {implementing} node addresses and any containers"),
        ));
    }
    Ok(())
}

fn check_managed_overlaps(
    cfg: &ClusterConfig,
    name: &str,
    at: &str,
    subnet: ipnet::Ipv4Net,
) -> Result<()> {
    for (other_name, other) in &cfg.networks {
        if other_name == name {
            continue;
        }
        let Some(ipnet::IpNet::V4(v4)) = other.subnet else {
            continue;
        };
        // A container network routing a wider range is fine: the more
        // specific managed subnet wins, like the underlay ranges do.
        if other.kind == NetworkKind::Container && v4.contains(&subnet.network()) && v4 != subnet {
            continue;
        }
        if overlaps(subnet, v4) {
            return Err(ConfigError::invalid(
                format!("{at}.subnet"),
                format!("{subnet} overlaps the subnet {v4} of network {other_name}"),
            ));
        }
    }
    Ok(())
}
