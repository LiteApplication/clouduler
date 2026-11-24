//! The host plumbing of managed private networks (see `cl_config::Network::managed`).
//!
//! A managed network is one cluster-wide L2 segment (built in `vxlan`) on the
//! nodes that implement it. Clients outside it route its whole subnet through
//! any implementing node, so each of those nodes also has to:
//!
//! * know how to reach a container another node runs: a /32 route per such
//!   container, through the tunnel to the hosting node (`managed_tunnel`), or
//!   failing that via its internal address ([`diff_routes`]);
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
    /// This node's source address towards another node through a tunnel (see
    /// `managed_tunnel::inner_addr`) and the tunnel devices to the others.
    pub own_inner: Option<Ipv4Addr>,
    pub tunnel_devs: Vec<String>,
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
    // Through a tunnel to another node: this node's inner address, which the
    // other node routes back through the same tunnel.
    if let Some(inner) = input.own_inner {
        for net in &input.nets {
            for dev in &input.tunnel_devs {
                nat.push(&[
                    "-d",
                    &net.subnet.to_string(),
                    "!",
                    "-s",
                    &net.subnet.to_string(),
                    "-o",
                    dev,
                    "-j",
                    "SNAT",
                    "--to-source",
                    &inner.to_string(),
                ]);
            }
        }
    }
    // To another node by a plain route (no tunnel): this node, so the reply
    // returns here.
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

/// Where a route sends its destination.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Hop {
    /// Through a tunnel device to the hosting node (see `managed_tunnel`).
    Dev(String),
    /// Via the hosting node's internal address: the fallback when no tunnel
    /// could be built, which only works where that network carries any
    /// destination (a LAN, not a WireGuard mesh).
    Via(Ipv4Addr),
}

/// What to change in the per-container routes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RouteDiff {
    /// Destination address and where to send it, new or changed.
    pub set: Vec<(Ipv4Addr, Hop)>,
    pub remove: Vec<Ipv4Addr>,
}

impl RouteDiff {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.remove.is_empty()
    }
}

/// `wanted` and `actual` map an address to its hop.
pub fn diff_routes(
    wanted: &BTreeMap<Ipv4Addr, Hop>,
    actual: &BTreeMap<Ipv4Addr, Hop>,
) -> RouteDiff {
    RouteDiff {
        set: wanted
            .iter()
            .filter(|(dst, hop)| actual.get(*dst) != Some(*hop))
            .map(|(dst, hop)| (*dst, hop.clone()))
            .collect(),
        remove: actual
            .keys()
            .filter(|dst| !wanted.contains_key(*dst))
            .copied()
            .collect(),
    }
}

/// Parse `ip -4 -o route show proto 201`: `192.168.32.5 dev clt-ab proto 201` or
/// `192.168.32.5 via 10.10.0.2 dev eth1 proto 201`.
pub fn parse_routes(listing: &str) -> BTreeMap<Ipv4Addr, Hop> {
    listing
        .lines()
        .filter_map(|line| {
            let mut tokens = line.split_whitespace();
            let dst: Ipv4Addr = tokens.next()?.trim_end_matches("/32").parse().ok()?;
            let rest: Vec<&str> = tokens.collect();
            let after = |key: &str| rest.windows(2).find(|w| w[0] == key).map(|w| w[1]);
            let hop = match after("via") {
                Some(via) => Hop::Via(via.parse().ok()?),
                None => Hop::Dev(after("dev")?.to_string()),
            };
            Some((dst, hop))
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
    for (dst, hop) in &diff.set {
        out.push(set_command(*dst, hop));
    }
    out
}

fn set_command(dst: Ipv4Addr, hop: &Hop) -> Vec<String> {
    let dst = format!("{dst}/32");
    let args: Vec<&str> = match hop {
        Hop::Dev(dev) => vec!["route", "replace", &dst, "dev", dev, "proto", ROUTE_PROTO],
        Hop::Via(via) => {
            return [
                "route",
                "replace",
                &dst,
                "via",
                &via.to_string(),
                "proto",
                ROUTE_PROTO,
            ]
            .iter()
            .map(|s| s.to_string())
            .collect()
        }
    };
    args.iter().map(|s| s.to_string()).collect()
}

/// The device `ip route get ADDR` reports (`10.10.0.2 dev eth1 src ...`).
pub fn parse_route_get_dev(listing: &str) -> Option<String> {
    let tokens: Vec<&str> = listing.split_whitespace().collect();
    tokens
        .windows(2)
        .find(|w| w[0] == "dev")
        .map(|w| w[1].to_string())
}

/// The same route as [`route_commands`] gives for `dst`, pinned to `dev` and
/// marked on-link so the kernel does not re-validate the next hop against the
/// main table (a next hop only a policy-routing table knows, as with tailscale,
/// is otherwise refused with "Nexthop has invalid gateway").
pub fn onlink_route_command(dst: Ipv4Addr, via: Ipv4Addr, dev: &str) -> Vec<String> {
    [
        "route",
        "replace",
        &format!("{dst}/32"),
        "via",
        &via.to_string(),
        "dev",
        dev,
        "onlink",
        "proto",
        ROUTE_PROTO,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Run `ip`, with `-4` when `v4`.
pub(crate) fn run_ip(args: &[&str], v4: bool) -> Result<String> {
    let mut cmd = Command::new("ip");
    if v4 {
        cmd.arg("-4");
    }
    let out = cmd.args(args).output().map_err(|e| NetError::Io {
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

fn ip(args: &[String]) -> Result<String> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_ip(&refs, true)
}

/// Make the host's per-container routes exactly `wanted`. Returns how many
/// were changed.
pub fn apply_routes(wanted: &BTreeMap<Ipv4Addr, Hop>) -> Result<usize> {
    let listing = ip(&[
        "-o".into(),
        "route".into(),
        "show".into(),
        "proto".into(),
        ROUTE_PROTO.into(),
    ])?;
    let diff = diff_routes(wanted, &parse_routes(&listing));
    let mut changed = 0;
    let mut failed: Vec<String> = Vec::new();
    // A route that vanished meanwhile, or a next hop that is not up yet, is
    // retried by the next pass.
    for command in route_commands(&RouteDiff {
        set: Vec::new(),
        remove: diff.remove.clone(),
    }) {
        match ip(&command) {
            Ok(_) => changed += 1,
            Err(e) => failed.push(e.to_string()),
        }
    }
    for (dst, hop) in &diff.set {
        match ip(&set_command(*dst, hop)) {
            Ok(_) => changed += 1,
            Err(e) => {
                // A `via` that the kernel refuses for its next hop is retried
                // pinned to the device that reaches it.
                let retried = match hop {
                    Hop::Via(via) => ip(&["route".into(), "get".into(), via.to_string()])
                        .ok()
                        .and_then(|got| parse_route_get_dev(&got))
                        .and_then(|dev| ip(&onlink_route_command(*dst, *via, &dev)).ok()),
                    Hop::Dev(_) => None,
                };
                match retried {
                    Some(_) => changed += 1,
                    None => failed.push(e.to_string()),
                }
            }
        }
    }
    // Without these routes a forwarded connection to a container on another
    // node leaves through the local bridge instead, so say so, once per
    // distinct set of failures rather than every pass.
    {
        static LAST: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let mut last = LAST.lock().unwrap_or_else(|p| p.into_inner());
        if *last != failed {
            if !failed.is_empty() {
                tracing::warn!(errors = ?failed, "managed network routes not applied");
            }
            *last = failed;
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
            ..Default::default()
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
        let via = |s: &str| Hop::Via(a(s));
        let wanted = BTreeMap::from([
            (a("192.168.32.5"), via("10.10.0.2")),
            (a("192.168.32.6"), via("10.10.0.3")),
            (a("192.168.32.8"), Hop::Dev("clt-1".into())),
        ]);
        let actual = BTreeMap::from([
            (a("192.168.32.6"), via("10.10.0.2")),
            (a("192.168.32.7"), via("10.10.0.2")),
        ]);
        let diff = diff_routes(&wanted, &actual);
        assert_eq!(
            diff.set,
            vec![
                (a("192.168.32.5"), via("10.10.0.2")),
                (a("192.168.32.6"), via("10.10.0.3")),
                (a("192.168.32.8"), Hop::Dev("clt-1".into()))
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
        assert_eq!(
            commands[3],
            [
                "route",
                "replace",
                "192.168.32.8/32",
                "dev",
                "clt-1",
                "proto",
                "201"
            ]
        );
    }

    #[test]
    fn onlink_fallback_is_pinned_to_the_device() {
        assert_eq!(
            parse_route_get_dev("10.10.0.2 dev tailscale0 table 52 src 10.10.0.1 uid 0 \n"),
            Some("tailscale0".to_string())
        );
        assert_eq!(
            parse_route_get_dev("RTNETLINK answers: Network is unreachable"),
            None
        );
        assert_eq!(
            onlink_route_command(a("192.168.32.5"), a("10.10.0.2"), "tailscale0"),
            [
                "route",
                "replace",
                "192.168.32.5/32",
                "via",
                "10.10.0.2",
                "dev",
                "tailscale0",
                "onlink",
                "proto",
                "201"
            ]
        );
        // The pinned route still parses as a per-container route.
        let routes = parse_routes("192.168.32.5 via 10.10.0.2 dev tailscale0 proto 201 onlink");
        assert_eq!(routes[&a("192.168.32.5")], Hop::Via(a("10.10.0.2")));
    }

    #[test]
    fn route_listings_parse() {
        let listing = "192.168.32.5 via 10.10.0.2 dev eth1 proto 201 \n192.168.32.6/32 via 10.10.0.3 dev eth1 proto 201\nbogus\n";
        let routes = parse_routes(listing);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[&a("192.168.32.6")], Hop::Via(a("10.10.0.3")));
        // A tunnel route has no next hop, only the device.
        let routes = parse_routes("192.168.32.9 dev clt-0123abcd scope link proto 201\n169.254.0.2 dev clt-0123abcd scope link proto 201");
        assert_eq!(routes[&a("192.168.32.9")], Hop::Dev("clt-0123abcd".into()));
        assert_eq!(routes.len(), 2);
    }

    #[test]
    fn tunnel_hops_are_natted_to_the_inner_address_before_the_plain_fallback() {
        let mut input = input();
        input.own_inner = Some(a("169.254.0.1"));
        input.tunnel_devs = vec!["clt-1".into()];
        let plan = managed_plan(&input);
        let nat = &plan.chain(NAT_CHAIN).unwrap().rules;
        assert_eq!(nat.len(), 3, "{nat:?}");
        assert!(
            nat[1].contains("-o clt-1") && nat[1].ends_with("--to-source 169.254.0.1"),
            "{nat:?}"
        );
        assert!(
            nat[2].contains("! -o") && nat[2].ends_with("--to-source 10.10.0.1"),
            "{nat:?}"
        );
    }
}
