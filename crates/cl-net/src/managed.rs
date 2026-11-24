//! The host firewall of managed private networks (see `cl_config::Network::managed`).
//!
//! A managed network is one cluster-wide L2 segment (built in `vxlan`) on the
//! nodes that implement it. Containers talk to each other over it and keep
//! their addresses, but nothing routes the subnet to the outside: an outside
//! client reaches a node `ADDRESS` on the network (a secondary address of the
//! node's bridge) and only the ports the network opens, which a userspace
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
//! Neither chain ever accepts: a verdict of `ACCEPT` in a sub-chain would skip
//! every later chain, so allowed traffic `RETURN`s and the others still apply.

use std::net::Ipv4Addr;

use cl_config::Protocol;
use ipnet::Ipv4Net;

use crate::error::Result;
use crate::iptables::{ChainSpec, IptablesPlan, Jump};

/// Filter chain, first in `DOCKER-USER` (else `FORWARD`).
pub const FORWARD_CHAIN: &str = "CLDR-MNG";

/// Filter chain, first in `INPUT`.
pub const INPUT_CHAIN: &str = "CLDR-MNG-IN";

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
    for net in &input.nets {
        let subnet = net.subnet.to_string();
        let bridge = net.bridge.as_str();
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
        let mut sources = net.sources.clone();
        sources.sort();
        sources.dedup();
        let mut ports = net.ports.clone();
        ports.sort();
        ports.dedup();
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
    if !inbound.rules.is_empty() {
        plan.chains.push(inbound);
        plan.jumps.push(
            Jump::new("filter", &["INPUT"], INPUT_CHAIN, true)
                .before(crate::egress::CONTAINER_INPUT_CHAIN),
        );
    }
    plan
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
        let _ = ip(&["route", "flush", "proto", LEGACY_ROUTE_PROTO], true);
    }
    let links = ip(&["-o", "link", "show", "type", "ipip"], false).unwrap_or_default();
    let tunnels = legacy_tunnels(&links);
    for dev in &tunnels {
        let _ = ip(&["link", "del", "dev", dev], false);
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
        assert_eq!(rules.len(), 4, "{rules:?}");
        assert!(rules[3].ends_with("DROP"));
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

    #[test]
    fn old_tunnel_devices_are_recognised() {
        let listing = "7: clt-0123abcd@NONE: <POINTOPOINT,NOARP,UP> mtu 1252 qdisc noqueue\n8: tunl0@NONE: <NOARP> mtu 1480\n9: other@NONE: <X>\n";
        assert_eq!(legacy_tunnels(listing), ["clt-0123abcd"]);
    }
}
