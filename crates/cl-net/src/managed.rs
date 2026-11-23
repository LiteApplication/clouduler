//! The host plumbing of managed private networks (see `cl_config::Network::managed`).
//!
//! A managed network is one cluster-wide L2 segment (built in `vxlan`) on the
//! nodes that implement it. Clients outside it route its whole subnet through
//! any implementing node, so each of those nodes also has to:
//!
//! * know how to reach a container another node runs: a /32 route per such
//!   container, via the hosting node's internal address ([`diff_routes`]);
//! * filter what is forwarded into the subnet ([`FORWARD_CHAIN`]): replies,
//!   the implementing nodes, the networks allowed to reach it, nothing else;
//! * source-NAT what it forwards ([`NAT_CHAIN`]). To another node it uses its own
//!   internal address, so the replies come back through it (and its
//!   conntrack undoes the translation). Into the bridge it uses the network's
//!   gateway, so a container answers through its managed interface whatever
//!   its default route is. A container therefore sees the gateway, not the
//!   client, as the source of such a connection.
//!
//! Everything is data first and unit tested without root, like the other
//! firewall modules. `FORWARD_CHAIN` accepts what it lets through (docker's
//! own rules would otherwise drop traffic routed into a bridge it owns) and
//! drops the rest of the subnet.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::process::Command;

use ipnet::Ipv4Net;

use crate::error::{NetError, Result};
use crate::iptables::{ChainSpec, IptablesPlan, Jump};

/// Filter chain, first in `DOCKER-USER` (else `FORWARD`).
pub const FORWARD_CHAIN: &str = "CLDR-MNG";

/// Nat chain, appended to `POSTROUTING`.
pub const NAT_CHAIN: &str = "CLDR-MNG-NAT";

/// Routing protocol the per-container routes carry, so the daemon only ever
/// lists and removes its own.
pub const ROUTE_PROTO: &str = "201";

/// One managed network this node implements, as far as the firewall cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedNetRules {
    pub name: String,
    pub subnet: Ipv4Net,
    /// The network's bridge on this node.
    pub bridge: String,
    pub gateway: Ipv4Addr,
    /// Subnets of the cluster networks allowed to reach this one
    /// (`ClusterConfig::reaches`).
    pub sources: Vec<Ipv4Net>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ManagedInput {
    pub nets: Vec<ManagedNetRules>,
    /// Internal addresses of every node implementing a managed network: the
    /// source of a connection forwarded from another node.
    pub node_addrs: Vec<Ipv4Addr>,
    /// This node's own internal address; without one no connection can be
    /// handed to another node.
    pub own_addr: Option<Ipv4Addr>,
}

/// The filter and nat chains. Empty when no managed network is implemented
/// here, which the daemon reads as "remove both".
pub fn managed_plan(input: &ManagedInput) -> IptablesPlan {
    if input.nets.is_empty() {
        return IptablesPlan::default();
    }
    let nodes: BTreeSet<Ipv4Addr> = input.node_addrs.iter().copied().collect();

    let mut forward = ChainSpec::new("filter", FORWARD_CHAIN);
    let mut nat = ChainSpec::new("nat", NAT_CHAIN);
    for net in &input.nets {
        let subnet = net.subnet.to_string();
        let bridge = net.bridge.as_str();
        // Replies to what a client opened, in both directions of the hop.
        forward.push(&[
            "-d",
            &subnet,
            "!",
            "-i",
            bridge,
            "-m",
            "conntrack",
            "--ctstate",
            "ESTABLISHED,RELATED",
            "-j",
            "ACCEPT",
        ]);
        forward.push(&[
            "-s",
            &subnet,
            "!",
            "-i",
            bridge,
            "-m",
            "conntrack",
            "--ctstate",
            "ESTABLISHED,RELATED",
            "-j",
            "ACCEPT",
        ]);
        for node in &nodes {
            forward.push(&[
                "-d",
                &subnet,
                "!",
                "-i",
                bridge,
                "-s",
                &format!("{node}/32"),
                "-j",
                "ACCEPT",
            ]);
        }
        let sources: BTreeSet<Ipv4Net> = net.sources.iter().copied().collect();
        for source in sources {
            forward.push(&[
                "-d",
                &subnet,
                "!",
                "-i",
                bridge,
                "-s",
                &source.to_string(),
                "-j",
                "ACCEPT",
            ]);
        }
        forward.push(&["-d", &subnet, "!", "-i", bridge, "-j", "DROP"]);

        // Into the bridge: the gateway, wherever the connection came from.
        nat.push(&[
            "-d",
            &subnet,
            "!",
            "-s",
            &subnet,
            "-o",
            bridge,
            "-j",
            "SNAT",
            "--to-source",
            &net.gateway.to_string(),
        ]);
    }
    // To another node: this node, so the reply returns here.
    if let Some(own) = input.own_addr {
        for net in &input.nets {
            let subnet = net.subnet.to_string();
            nat.push(&[
                "-d",
                &subnet,
                "!",
                "-s",
                &subnet,
                "!",
                "-o",
                &net.bridge,
                "-j",
                "SNAT",
                "--to-source",
                &own.to_string(),
            ]);
        }
    }

    IptablesPlan {
        chains: vec![forward, nat],
        jumps: vec![
            Jump::new("filter", &["DOCKER-USER", "FORWARD"], FORWARD_CHAIN, true),
            Jump::new("nat", &["POSTROUTING"], NAT_CHAIN, false),
        ],
    }
}

/// What to change in the per-container routes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RouteDiff {
    /// Container address and the node to send it through, new or changed.
    pub set: Vec<(Ipv4Addr, Ipv4Addr)>,
    pub remove: Vec<Ipv4Addr>,
}

impl RouteDiff {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.remove.is_empty()
    }
}

/// `wanted` and `actual` map a container address to the internal address of
/// the node hosting it.
pub fn diff_routes(
    wanted: &BTreeMap<Ipv4Addr, Ipv4Addr>,
    actual: &BTreeMap<Ipv4Addr, Ipv4Addr>,
) -> RouteDiff {
    RouteDiff {
        set: wanted
            .iter()
            .filter(|(dst, via)| actual.get(*dst) != Some(*via))
            .map(|(dst, via)| (*dst, *via))
            .collect(),
        remove: actual
            .keys()
            .filter(|dst| !wanted.contains_key(*dst))
            .copied()
            .collect(),
    }
}

/// Parse `ip -4 -o route show proto 201`: `192.168.32.5 via 10.10.0.2 dev eth1 proto 201`.
pub fn parse_routes(listing: &str) -> BTreeMap<Ipv4Addr, Ipv4Addr> {
    listing
        .lines()
        .filter_map(|line| {
            let mut tokens = line.split_whitespace();
            let dst: Ipv4Addr = tokens.next()?.trim_end_matches("/32").parse().ok()?;
            let rest: Vec<&str> = tokens.collect();
            let via = rest.windows(2).find(|w| w[0] == "via")?[1].parse().ok()?;
            Some((dst, via))
        })
        .collect()
}

/// The `ip` argument lists applying `diff`.
pub fn route_commands(diff: &RouteDiff) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for dst in &diff.remove {
        out.push(
            ["route", "del", &format!("{dst}/32"), "proto", ROUTE_PROTO]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
    }
    for (dst, via) in &diff.set {
        out.push(
            [
                "route",
                "replace",
                &format!("{dst}/32"),
                "via",
                &via.to_string(),
                "proto",
                ROUTE_PROTO,
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        );
    }
    out
}

fn ip(args: &[String]) -> Result<String> {
    let out = Command::new("ip")
        .arg("-4")
        .args(args)
        .output()
        .map_err(|e| NetError::Io {
            context: "running ip".to_string(),
            source: e,
        })?;
    if !out.status.success() {
        return Err(NetError::Io {
            context: format!("ip {}", args.join(" ")),
            source: std::io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Make the host's per-container routes exactly `wanted`. Returns how many
/// were changed.
pub fn apply_routes(wanted: &BTreeMap<Ipv4Addr, Ipv4Addr>) -> Result<usize> {
    let listing = ip(&[
        "-o".into(),
        "route".into(),
        "show".into(),
        "proto".into(),
        ROUTE_PROTO.into(),
    ])?;
    let diff = diff_routes(wanted, &parse_routes(&listing));
    let mut changed = 0;
    for command in route_commands(&diff) {
        // A route that vanished meanwhile, or a next hop that is not up yet,
        // is retried by the next pass.
        match ip(&command) {
            Ok(_) => changed += 1,
            Err(e) => tracing::debug!(error = %e, "managed network route not applied"),
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    fn input() -> ManagedInput {
        ManagedInput {
            nets: vec![ManagedNetRules {
                name: "media".into(),
                subnet: "192.168.32.0/24".parse().unwrap(),
                bridge: "clbr-@media".into(),
                gateway: a("192.168.32.1"),
                sources: vec!["100.64.0.0/10".parse().unwrap()],
            }],
            node_addrs: vec![a("10.10.0.2"), a("10.10.0.1")],
            own_addr: Some(a("10.10.0.1")),
        }
    }

    #[test]
    fn no_managed_network_means_no_chains() {
        assert_eq!(
            managed_plan(&ManagedInput::default()),
            IptablesPlan::default()
        );
    }

    #[test]
    fn the_filter_accepts_replies_nodes_and_allowed_networks_then_drops() {
        let plan = managed_plan(&input());
        let rules = &plan.chain(FORWARD_CHAIN).unwrap().rules;
        assert!(
            rules[0].contains("-d 192.168.32.0/24 ! -i clbr-@media")
                && rules[0].contains("ESTABLISHED")
        );
        assert!(rules[1].starts_with("-s 192.168.32.0/24") && rules[1].ends_with("ACCEPT"));
        assert!(rules.contains(
            &"-d 192.168.32.0/24 ! -i clbr-@media -s 10.10.0.1/32 -j ACCEPT".to_string()
        ));
        assert!(rules.contains(
            &"-d 192.168.32.0/24 ! -i clbr-@media -s 10.10.0.2/32 -j ACCEPT".to_string()
        ));
        assert!(rules.contains(
            &"-d 192.168.32.0/24 ! -i clbr-@media -s 100.64.0.0/10 -j ACCEPT".to_string()
        ));
        assert_eq!(
            rules.last().unwrap(),
            "-d 192.168.32.0/24 ! -i clbr-@media -j DROP"
        );
        // The default verdict is last, so no accept follows it.
        assert!(rules[..rules.len() - 1]
            .iter()
            .all(|r| !r.ends_with("DROP")));
    }

    #[test]
    fn a_source_nobody_listed_is_not_accepted() {
        let plan = managed_plan(&input());
        let rules = &plan.chain(FORWARD_CHAIN).unwrap().rules;
        assert!(!rules.iter().any(|r| r.contains("192.168.1.0")));
    }

    #[test]
    fn nat_uses_the_gateway_into_the_bridge_and_this_node_towards_the_others() {
        let plan = managed_plan(&input());
        let rules = &plan.chain(NAT_CHAIN).unwrap().rules;
        assert_eq!(
            rules[0],
            "-d 192.168.32.0/24 ! -s 192.168.32.0/24 -o clbr-@media -j SNAT --to-source 192.168.32.1"
        );
        assert_eq!(
            rules[1],
            "-d 192.168.32.0/24 ! -s 192.168.32.0/24 ! -o clbr-@media -j SNAT --to-source 10.10.0.1"
        );
    }

    #[test]
    fn without_an_own_address_nothing_is_handed_to_another_node() {
        let mut i = input();
        i.own_addr = None;
        let plan = managed_plan(&i);
        assert_eq!(plan.chain(NAT_CHAIN).unwrap().rules.len(), 1);
    }

    #[test]
    fn the_jumps_land_in_docker_user_first_and_postrouting() {
        let plan = managed_plan(&input());
        assert_eq!(plan.jumps[0].parents, vec!["DOCKER-USER", "FORWARD"]);
        assert!(plan.jumps[0].first);
        assert_eq!(plan.jumps[1].parents, vec!["POSTROUTING"]);
    }

    #[test]
    fn routes_diff_adds_changes_and_removes() {
        let wanted = BTreeMap::from([
            (a("192.168.32.5"), a("10.10.0.2")),
            (a("192.168.32.6"), a("10.10.0.3")),
        ]);
        let actual = BTreeMap::from([
            (a("192.168.32.6"), a("10.10.0.2")),
            (a("192.168.32.7"), a("10.10.0.2")),
        ]);
        let diff = diff_routes(&wanted, &actual);
        assert_eq!(
            diff.set,
            vec![
                (a("192.168.32.5"), a("10.10.0.2")),
                (a("192.168.32.6"), a("10.10.0.3"))
            ]
        );
        assert_eq!(diff.remove, vec![a("192.168.32.7")]);
        assert!(diff_routes(&wanted, &wanted).is_empty());
        let commands = route_commands(&diff);
        assert_eq!(
            commands[0],
            ["route", "del", "192.168.32.7/32", "proto", "201"]
        );
        assert_eq!(
            commands[1],
            [
                "route",
                "replace",
                "192.168.32.5/32",
                "via",
                "10.10.0.2",
                "proto",
                "201"
            ]
        );
    }

    #[test]
    fn route_listings_parse() {
        let listing = "192.168.32.5 via 10.10.0.2 dev eth1 proto 201 \n192.168.32.6/32 via 10.10.0.3 dev eth1 proto 201\nbogus\n";
        let routes = parse_routes(listing);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[&a("192.168.32.6")], a("10.10.0.3"));
    }
}
