//! The host firewall of managed private networks (see `cl_config::Network::managed`).
//!
//! A managed network is one cluster-wide L2 segment (built in `vxlan`) on the
//! nodes that implement it. Containers talk to each other over it and keep
//! their addresses, but nothing routes the subnet to the outside: an outside
//! client reaches a node `ADDRESS` on the network (on a port of its own, see
//! below) and only the ports the network opens, which a userspace
//! forwarder hands to the container running on that very node.
//!
//! Two chains, both data first and unit tested without root:
//!
//! * [`INPUT_CHAIN`] (filter `INPUT`): what may reach a node address. The
//!   host's own and the bridge's traffic is left to the other chains, replies
//!   pass, the open ports (and 80/443 when the network serves the proxy) are
//!   let through for the source networks whose `access` lists this network,
//!   everything else to that address is dropped. A source in no cluster network
//!   is in nobody's list, so it is dropped too.
//! * [`FORWARD_CHAIN`] (filter `DOCKER-USER`, else `FORWARD`): nothing is routed into
//!   the subnet from outside its bridge, apart from replies.
//!
//! The node address lives on a **node port** (see [`port_veth_name`]), a veth
//! whose peer is enslaved to the bridge, with a MAC of its own
//! ([`node_port_mac`]) instead of a secondary address on the bridge (whose MAC
//! is the anycast gateway one that never crosses the VXLAN). Every node
//! therefore answers ARP for its own node address with a unique MAC and the
//! node addresses reach each other over the tunnel. That makes one more case
//! possible: a packet that enters a node from outside for **another** node's
//! address is routed out the node port (a /32 route per peer address, see
//! [`route_commands`]), source NATed to this node's own address
//! ([`SNAT_CHAIN`]) so the reply comes back to this node's MAC, and let
//! through by [`FORWARD_CHAIN`] only for the open ports, 80/443 with the proxy
//! and ping, from the source networks that may reach the network. Nothing
//! else is ever routed into the subnet, container addresses included.
//!
//! Apart from that one forwarding case (`ACCEPT`, since the veth is not
//! docker's and the `FORWARD` policy would drop it) neither chain accepts: a
//! verdict of `ACCEPT` in a sub-chain would skip every later chain, so
//! allowed traffic `RETURN`s and the others still apply.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use cl_config::Protocol;
use ipnet::Ipv4Net;

use crate::error::Result;
use crate::iptables::{ChainSpec, IptablesPlan, Jump};

/// Filter chain, first in `DOCKER-USER` (else `FORWARD`).
pub const FORWARD_CHAIN: &str = "CLDR-MNG";

/// Filter chain, first in `INPUT`.
pub const INPUT_CHAIN: &str = "CLDR-MNG-IN";

/// Nat chain (`POSTROUTING`): source NAT of traffic forwarded to another node's address.
pub const SNAT_CHAIN: &str = "CLDR-MNG-SNAT";

/// Nat chain of earlier versions (cross-node source NAT), removed when found.
pub const LEGACY_NAT_CHAIN: &str = "CLDR-MNG-NAT";

/// Routing protocol the per-container routes of earlier versions carried.
const LEGACY_ROUTE_PROTO: &str = "201";

/// Prefix of the IPIP tunnel devices of earlier versions.
const LEGACY_TUNNEL_PREFIX: &str = "clt-";

/// UDP port of the FOU receiver of earlier versions.
const LEGACY_FOU_PORT: &str = "7390";

/// One managed network this node implements, as far as the firewall cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedNetRules {
    pub name: String,
    pub subnet: Ipv4Net,
    /// The network's bridge on this node.
    pub bridge: String,
    /// The node port holding the node address (`port_veth_name`).
    pub port: String,
    /// The addresses of the other nodes on the network.
    pub peers: Vec<Ipv4Addr>,
    /// This node's address on the network; without one nothing can reach the node there.
    pub node_addr: Option<Ipv4Addr>,
    /// Subnets of the cluster networks allowed to reach this one
    /// (`ClusterConfig::reaches`).
    pub sources: Vec<Ipv4Net>,
    /// What is reachable on the node address: the open ports, plus 80/443 when
    /// the network serves the reverse proxy.
    pub ports: Vec<(Protocol, u16)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ManagedInput {
    pub nets: Vec<ManagedNetRules>,
}

/// The filter chains. Empty when no managed network is implemented here, which
/// the daemon reads as "remove both". The input chain is left out when no
/// network has a node address on this node.
pub fn managed_plan(input: &ManagedInput) -> IptablesPlan {
    if input.nets.is_empty() {
        return IptablesPlan::default();
    }
    let mut forward = ChainSpec::new("filter", FORWARD_CHAIN);
    let mut inbound = ChainSpec::new("filter", INPUT_CHAIN);
    let mut snat = ChainSpec::new("nat", SNAT_CHAIN);
    for net in &input.nets {
        let subnet = net.subnet.to_string();
        let bridge = net.bridge.as_str();
        let port = net.port.as_str();
        let mut peers = net.peers.clone();
        peers.sort();
        peers.dedup();
        let mut sources = net.sources.clone();
        sources.sort();
        sources.dedup();
        let mut ports = net.ports.clone();
        ports.sort();
        ports.dedup();
        // Traffic that entered this node for another node's address: only the
        // open ports and ping, from the source networks that may reach this
        // one, out the node port. The replies come back the same way.
        if let Some(addr) = net.node_addr {
            let src_nat = addr.to_string();
            for peer in &peers {
                let dst = format!("{peer}/32");
                forward.push(&[
                    "-s",
                    &dst,
                    "-i",
                    port,
                    "-m",
                    "conntrack",
                    "--ctstate",
                    "ESTABLISHED,RELATED",
                    "-j",
                    "ACCEPT",
                ]);
                for source in &sources {
                    let source = source.to_string();
                    forward.push(&[
                        "-s",
                        &source,
                        "-d",
                        &dst,
                        "-o",
                        port,
                        "-p",
                        "icmp",
                        "--icmp-type",
                        "echo-request",
                        "-j",
                        "ACCEPT",
                    ]);
                    for (protocol, number) in &ports {
                        forward.push(&[
                            "-s",
                            &source,
                            "-d",
                            &dst,
                            "-o",
                            port,
                            "-p",
                            &protocol.to_string(),
                            "--dport",
                            &number.to_string(),
                            "-j",
                            "ACCEPT",
                        ]);
                    }
                }
            }
            if !peers.is_empty() {
                snat.push(&[
                    "!",
                    "-s",
                    &subnet,
                    "-d",
                    &subnet,
                    "-o",
                    port,
                    "-j",
                    "SNAT",
                    "--to-source",
                    &src_nat,
                ]);
            }
        }
        // Replies to what a container opened outwards.
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
            "RETURN",
        ]);
        forward.push(&["-d", &subnet, "!", "-i", bridge, "-j", "DROP"]);

        let Some(addr) = net.node_addr else {
            continue;
        };
        let dst = format!("{addr}/32");
        // The host itself and the containers of the network: other chains decide.
        inbound.push(&["-d", &dst, "-i", bridge, "-j", "RETURN"]);
        inbound.push(&["-d", &dst, "-i", "lo", "-j", "RETURN"]);
        inbound.push(&[
            "-d",
            &dst,
            "-m",
            "conntrack",
            "--ctstate",
            "ESTABLISHED,RELATED",
            "-j",
            "RETURN",
        ]);
        // Another node's address as the source: traffic that entered that node
        // from outside and was source NATed there (its source networks were
        // checked on that node). Only the open ports and ping.
        for peer in &peers {
            let from = format!("{peer}/32");
            inbound.push(&[
                "-s",
                &from,
                "-d",
                &dst,
                "-i",
                port,
                "-p",
                "icmp",
                "--icmp-type",
                "echo-request",
                "-j",
                "RETURN",
            ]);
            for (protocol, number) in &ports {
                inbound.push(&[
                    "-s",
                    &from,
                    "-d",
                    &dst,
                    "-i",
                    port,
                    "-p",
                    &protocol.to_string(),
                    "--dport",
                    &number.to_string(),
                    "-j",
                    "RETURN",
                ]);
            }
            inbound.push(&["-s", &from, "-d", &dst, "-i", port, "-j", "DROP"]);
        }
        // Containers (their frames to the node address arrive through the port).
        inbound.push(&["-d", &dst, "-i", port, "-j", "RETURN"]);
        for source in &sources {
            let source = source.to_string();
            inbound.push(&[
                "-s",
                &source,
                "-d",
                &dst,
                "-p",
                "icmp",
                "--icmp-type",
                "echo-request",
                "-j",
                "RETURN",
            ]);
            for (protocol, port) in &ports {
                inbound.push(&[
                    "-s",
                    &source,
                    "-d",
                    &dst,
                    "-p",
                    &protocol.to_string(),
                    "--dport",
                    &port.to_string(),
                    "-j",
                    "RETURN",
                ]);
            }
        }
        inbound.push(&["-d", &dst, "-j", "DROP"]);
    }

    let mut plan = IptablesPlan {
        chains: vec![forward],
        jumps: vec![Jump::new(
            "filter",
            &["DOCKER-USER", "FORWARD"],
            FORWARD_CHAIN,
            true,
        )],
    };
    if !snat.rules.is_empty() {
        plan.chains.push(snat);
        plan.jumps
            .push(Jump::new("nat", &["POSTROUTING"], SNAT_CHAIN, true));
    }
    if !inbound.rules.is_empty() {
        plan.chains.push(inbound);
        plan.jumps.push(
            Jump::new("filter", &["INPUT"], INPUT_CHAIN, true)
                .before(crate::egress::CONTAINER_INPUT_CHAIN),
        );
    }
    plan
}

/// Name of the node port (host end of the veth carrying the node address) of
/// a network, from its bridge key (`managed_key`).
pub fn port_veth_name(key: &str) -> String {
    crate::egress::short_name("clmh-", key, 15)
}

/// The end of the node port enslaved to the network's bridge.
pub fn port_peer_name(key: &str) -> String {
    crate::egress::short_name("clmp-", key, 15)
}

/// MAC of a node's port on a managed network: locally administered unicast,
/// unique per `(network, node)`, and never the `0x02` anycast gateway MAC
/// (which the tunnel keeps local), so node addresses reach each other over
/// the VXLAN.
pub fn node_port_mac(key: &str, node: &str) -> [u8; 6] {
    crate::container_net::host_port_mac(key, node)
}

/// The `ip` commands that make the /32 routes of `peers` via `port`, from
/// `src`, exactly the `current` ones (peer addresses already routed there).
pub fn route_commands(
    port: &str,
    src: Ipv4Addr,
    peers: &BTreeSet<Ipv4Addr>,
    current: &BTreeSet<Ipv4Addr>,
) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for stale in current.difference(peers) {
        out.push(
            ["route", "del", &format!("{stale}/32"), "dev", port]
                .map(String::from)
                .to_vec(),
        );
    }
    for missing in peers.difference(current) {
        out.push(
            [
                "route",
                "replace",
                &format!("{missing}/32"),
                "dev",
                port,
                "src",
                &src.to_string(),
                "scope",
                "link",
            ]
            .map(String::from)
            .to_vec(),
        );
    }
    out
}

/// The host routes `ip -4 -o route show dev PORT` lists (`a.b.c.d scope link`
/// or `a.b.c.d/32 ...`).
pub fn parse_routes(listing: &str) -> BTreeSet<Ipv4Addr> {
    listing
        .lines()
        .filter_map(|l| {
            let dest = l.split_whitespace().next()?;
            dest.strip_suffix("/32").unwrap_or(dest).parse().ok()
        })
        .collect()
}

/// Route the addresses of the other nodes through the node port. Blocking.
pub fn reconcile_peer_routes(port: &str, src: Ipv4Addr, peers: &BTreeSet<Ipv4Addr>) -> Result<()> {
    let listing = ip(&["-o", "route", "show", "dev", port], true).unwrap_or_default();
    for command in route_commands(port, src, peers, &parse_routes(&listing)) {
        let args: Vec<&str> = command.iter().map(String::as_str).collect();
        ip(&args, true)?;
    }
    Ok(())
}

fn ip(args: &[&str], v4: bool) -> Result<String> {
    let mut cmd = std::process::Command::new("ip");
    if v4 {
        cmd.arg("-4");
    }
    let out = cmd
        .args(args)
        .output()
        .map_err(|e| crate::error::NetError::Io {
            context: "running ip".to_string(),
            source: e,
        })?;
    if !out.status.success() {
        return Err(crate::error::NetError::Io {
            context: format!("ip {}", args.join(" ")),
            source: std::io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The devices `ip -o link show type ipip` lists that earlier versions made
/// (`7: clt-ab@NONE: <...>`).
pub fn legacy_tunnels(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|l| {
            let name = l.split_whitespace().nth(1)?;
            let name = name.trim_end_matches(':').split('@').next()?;
            name.starts_with(LEGACY_TUNNEL_PREFIX)
                .then(|| name.to_string())
        })
        .collect()
}

/// Remove what earlier versions left on the host: the per-container routes
/// (protocol 201), the IPIP tunnels and their FOU receiver. Every step is
/// best effort and silent when there is nothing to remove.
pub fn remove_legacy_plumbing() {
    let routes =
        ip(&["-o", "route", "show", "proto", LEGACY_ROUTE_PROTO], true).unwrap_or_default();
    if !routes.trim().is_empty() {
        if let Err(e) = ip(&["route", "flush", "proto", LEGACY_ROUTE_PROTO], true) {
            tracing::warn!(error = %e, "could not flush the legacy routes");
        }
    }
    let links = ip(&["-o", "link", "show", "type", "ipip"], false).unwrap_or_default();
    let tunnels = legacy_tunnels(&links);
    for dev in &tunnels {
        if let Err(e) = ip(&["link", "del", "dev", dev], false) {
            tracing::warn!(error = %e, "could not delete a legacy tunnel");
        }
    }
    if !tunnels.is_empty() {
        let _ = ip(&["fou", "del", "port", LEGACY_FOU_PORT], false);
    }
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
                port: "clmh-@media".into(),
                peers: Vec::new(),
                node_addr: Some(a("192.168.32.254")),
                sources: vec![
                    "100.64.0.0/10".parse().unwrap(),
                    "192.168.1.0/24".parse().unwrap(),
                ],
                ports: vec![(Protocol::Tcp, 8096), (Protocol::Udp, 1900)],
            }],
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
    fn nothing_is_routed_into_the_subnet_from_outside_but_replies() {
        let plan = managed_plan(&input());
        let rules = &plan.chain(FORWARD_CHAIN).unwrap().rules;
        assert_eq!(rules.len(), 2);
        assert!(rules[0].contains("ESTABLISHED") && rules[0].ends_with("RETURN"));
        assert_eq!(rules[1], "-d 192.168.32.0/24 ! -i clbr-@media -j DROP");
        // Neither chain accepts.
        for chain in &plan.chains {
            assert!(
                chain.rules.iter().all(|r| !r.ends_with("ACCEPT")),
                "{chain:?}"
            );
        }
    }

    #[test]
    fn the_node_address_answers_only_open_ports_for_listed_sources() {
        let plan = managed_plan(&input());
        let rules = &plan.chain(INPUT_CHAIN).unwrap().rules;
        let dst = "192.168.32.254/32";
        assert_eq!(rules[0], format!("-d {dst} -i clbr-@media -j RETURN"));
        assert_eq!(rules[1], format!("-d {dst} -i lo -j RETURN"));
        assert!(rules[2].contains("ESTABLISHED"));
        for source in ["100.64.0.0/10", "192.168.1.0/24"] {
            assert!(rules.contains(&format!(
                "-s {source} -d {dst} -p tcp --dport 8096 -j RETURN"
            )));
            assert!(rules.contains(&format!(
                "-s {source} -d {dst} -p udp --dport 1900 -j RETURN"
            )));
        }
        assert!(!rules.iter().any(|r| r.contains("--dport 22")));
        assert_eq!(rules.last().unwrap(), &format!("-d {dst} -j DROP"));
        assert!(rules[..rules.len() - 1]
            .iter()
            .all(|r| !r.ends_with("DROP")));
    }

    #[test]
    fn no_listed_source_leaves_only_the_drop() {
        let mut i = input();
        i.nets[0].sources.clear();
        let plan = managed_plan(&i);
        let rules = &plan.chain(INPUT_CHAIN).unwrap().rules;
        assert_eq!(rules.len(), 5, "{rules:?}");
        assert_eq!(rules[3], "-d 192.168.32.254/32 -i clmh-@media -j RETURN");
        assert!(rules[4].ends_with("DROP"));
    }

    #[test]
    fn a_node_without_an_address_has_no_input_chain() {
        let mut i = input();
        i.nets[0].node_addr = None;
        let plan = managed_plan(&i);
        assert!(plan.chain(INPUT_CHAIN).is_none());
        assert_eq!(plan.jumps.len(), 1);
    }

    #[test]
    fn the_jumps_land_in_docker_user_and_input_first() {
        let plan = managed_plan(&input());
        assert_eq!(plan.jumps[0].parents, vec!["DOCKER-USER", "FORWARD"]);
        assert!(plan.jumps[0].first);
        assert_eq!(plan.jumps[1].parents, vec!["INPUT"]);
        assert!(plan.jumps[1].first);
    }

    fn with_peers() -> ManagedInput {
        let mut i = input();
        i.nets[0].peers = vec![a("192.168.32.253"), a("192.168.32.121")];
        i
    }

    #[test]
    fn a_remote_node_address_is_forwarded_only_for_open_ports_from_listed_sources() {
        let plan = managed_plan(&with_peers());
        let rules = &plan.chain(FORWARD_CHAIN).unwrap().rules;
        let peer = "192.168.32.121/32";
        assert!(rules.contains(&format!(
            "-s 192.168.1.0/24 -d {peer} -o clmh-@media -p tcp --dport 8096 -j ACCEPT"
        )));
        assert!(rules.contains(&format!(
            "-s 100.64.0.0/10 -d {peer} -o clmh-@media -p icmp --icmp-type echo-request -j ACCEPT"
        )));
        assert!(rules.contains(&format!(
            "-s {peer} -i clmh-@media -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT"
        )));
        // Nothing but ports 8096/1900: no ssh, and no container address.
        assert!(!rules.iter().any(|r| r.contains("--dport 22")));
        assert!(rules
            .iter()
            .filter(|r| r.ends_with("ACCEPT"))
            .all(|r| r.contains("clmh-@media")));
        // The drop for everything else into the subnet stays, after the allowances.
        let drop = rules
            .iter()
            .position(|r| r == "-d 192.168.32.0/24 ! -i clbr-@media -j DROP")
            .unwrap();
        assert!(
            rules[..drop]
                .iter()
                .filter(|r| r.ends_with("ACCEPT"))
                .count()
                > 0
        );
        assert!(rules[drop + 1..].is_empty());
    }

    #[test]
    fn forwarded_traffic_is_source_nated_to_this_nodes_address() {
        let plan = managed_plan(&with_peers());
        let nat = plan.chain(SNAT_CHAIN).unwrap();
        assert_eq!(nat.table, "nat");
        assert_eq!(
            nat.rules,
            ["! -s 192.168.32.0/24 -d 192.168.32.0/24 -o clmh-@media -j SNAT --to-source 192.168.32.254"]
        );
        let jump = plan.jumps.iter().find(|j| j.chain == SNAT_CHAIN).unwrap();
        assert_eq!(
            (jump.table, jump.parents.clone()),
            ("nat", vec!["POSTROUTING".to_string()])
        );
        // Alone on the network, or with no address: nothing to translate.
        assert!(managed_plan(&input()).chain(SNAT_CHAIN).is_none());
    }

    #[test]
    fn the_owner_trusts_a_peer_node_address_for_the_open_ports_only() {
        let plan = managed_plan(&with_peers());
        let rules = &plan.chain(INPUT_CHAIN).unwrap().rules;
        let dst = "192.168.32.254/32";
        let peer = "192.168.32.121/32";
        assert!(rules.contains(&format!(
            "-s {peer} -d {dst} -i clmh-@media -p tcp --dport 8096 -j RETURN"
        )));
        let drop = rules
            .iter()
            .position(|r| r == &format!("-s {peer} -d {dst} -i clmh-@media -j DROP"))
            .unwrap();
        let containers = rules
            .iter()
            .position(|r| r == &format!("-d {dst} -i clmh-@media -j RETURN"))
            .unwrap();
        assert!(drop < containers);
    }

    #[test]
    fn peer_routes_go_through_the_node_port() {
        let peers: BTreeSet<Ipv4Addr> = [a("192.168.32.121"), a("192.168.32.2")].into();
        let current = parse_routes(
            "192.168.32.2 scope link src 192.168.32.254\n192.168.32.9/32 scope link\n",
        );
        assert_eq!(current, [a("192.168.32.2"), a("192.168.32.9")].into());
        let cmds = route_commands("clmh-@media", a("192.168.32.254"), &peers, &current);
        assert_eq!(
            cmds,
            vec![
                vec!["route", "del", "192.168.32.9/32", "dev", "clmh-@media"],
                vec![
                    "route",
                    "replace",
                    "192.168.32.121/32",
                    "dev",
                    "clmh-@media",
                    "src",
                    "192.168.32.254",
                    "scope",
                    "link"
                ],
            ]
        );
        assert!(route_commands("p", a("1.1.1.1"), &peers, &peers).is_empty());
    }

    #[test]
    fn the_node_port_mac_is_unique_per_node_and_never_the_anycast_one() {
        let m = node_port_mac("@media", "n1");
        assert_eq!(m[0] & 0x03, 0x02, "locally administered unicast");
        assert_eq!(m, node_port_mac("@media", "n1"));
        assert_ne!(m, node_port_mac("@media", "n2"));
        assert_ne!(m, node_port_mac("@other", "n1"));
        assert_ne!(m, crate::vxlan::gateway_mac("clbr-@media"));
        assert_ne!(m[0], crate::vxlan::gateway_mac("clbr-@media")[0]);
        assert_eq!(port_veth_name("@media"), "clmh-@media");
        assert!(port_veth_name("@a-very-long-network-name").len() <= 15);
        assert_ne!(port_veth_name("@media"), port_peer_name("@media"));
    }

    #[test]
    fn old_tunnel_devices_are_recognised() {
        let listing = "7: clt-0123abcd@NONE: <POINTOPOINT,NOARP,UP> mtu 1252 qdisc noqueue\n8: tunl0@NONE: <NOARP> mtu 1480\n9: other@NONE: <X>\n";
        assert_eq!(legacy_tunnels(listing), ["clt-0123abcd"]);
    }
}
