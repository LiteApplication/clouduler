//! The host L3 firewall behind `Network.access`.
//!
//! A subnetted (internal or private) network `A` with an `access` list may
//! only reach the subnetted networks it lists. Everything else is dropped on
//! this host, in two chains:
//!
//! * `CLDR-NET` (filter `INPUT`): traffic from `A`'s subnet to this node's own
//!   address on a denied network `B`. Jumped from the top of `INPUT`, ahead of
//!   `CLDR-IN` (its `ESTABLISHED` accept and its port rules would otherwise
//!   pre-empt the drop).
//! * `CLDR-NETF` (filter `FORWARD`): traffic routed between the two subnets.
//!   Hooked exactly like the other forward chains (`CLDR-ALLOW`, `CLDR-CTNF-*`,
//!   the egress chains): into `DOCKER-USER`, else `FORWARD`, at the top, so
//!   docker's own accept rules cannot pre-empt it.
//!
//! Both chains only ever `RETURN` or `DROP`, never `ACCEPT`: an accept in a
//! sub-chain would skip every later chain (`CLDR-IN` first of all) and turn a
//! permissive pass into a bypass of the other filters.
//!
//! Only IPv4 subnets are handled; the firewall layer in this crate is
//! iptables only. The caller leaves IPv6 subnets out of the input.
//!
//! No source network with an `access` list, or nothing denied, gives an empty
//! plan, which the daemon reads as "remove both chains".

use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use ipnet::Ipv4Net;

use crate::iptables::{ChainSpec, IptablesPlan, Jump};

pub const INPUT_CHAIN: &str = "CLDR-NET";
pub const FORWARD_CHAIN: &str = "CLDR-NETF";

/// A network a source is denied from reaching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    pub network: String,
    pub subnet: Ipv4Net,
    /// This node's own address on the network, if it has one.
    pub node_addr: Option<Ipv4Addr>,
}

/// One restricted source network and everything it is denied from reaching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRules {
    pub network: String,
    pub subnet: Ipv4Net,
    pub denied: Vec<Denied>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetAccessInput {
    /// The dashboard's direct listener, always reachable.
    pub web_port: u16,
    pub sources: Vec<SourceRules>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetAccessPlan {
    pub plan: IptablesPlan,
    /// Pairs that could not be enforced, one human readable line each.
    pub warnings: Vec<String>,
}

pub fn net_access_plan(input: &NetAccessInput) -> NetAccessPlan {
    let mut input_rules: Vec<String> = Vec::new();
    let mut forward_rules: Vec<String> = Vec::new();
    let mut warnings = Vec::new();
    let mut seen_input = BTreeSet::new();
    let mut seen_forward = BTreeSet::new();

    for source in &input.sources {
        for denied in &source.denied {
            if overlaps(source.subnet, denied.subnet) {
                warnings.push(format!(
                    "network access {} -> {} not enforced: subnets {} and {} overlap",
                    source.network, denied.network, source.subnet, denied.subnet
                ));
                continue;
            }
            if let Some(addr) = denied.node_addr {
                let rule = format!("-s {} -d {addr}/32 -j DROP", source.subnet);
                if seen_input.insert(rule.clone()) {
                    input_rules.push(rule);
                }
            }
            let rule = format!("-s {} -d {} -j DROP", source.subnet, denied.subnet);
            if seen_forward.insert(rule.clone()) {
                forward_rules.push(rule);
            }
        }
    }

    if forward_rules.is_empty() && input_rules.is_empty() {
        return NetAccessPlan {
            plan: IptablesPlan::default(),
            warnings,
        };
    }

    let mut input_chain = ChainSpec::new("filter", INPUT_CHAIN);
    // Replies to connections the allowed side opened must keep flowing. RETURN
    // (not ACCEPT) so the later chains still apply to them.
    input_chain.push(&[
        "-m",
        "conntrack",
        "--ctstate",
        "ESTABLISHED,RELATED",
        "-j",
        "RETURN",
    ]);
    // Host-local traffic between this node's own addresses arrives on lo.
    input_chain.push(&["-i", "lo", "-j", "RETURN"]);
    input_chain.push(&[
        "-p",
        "tcp",
        "--dport",
        &input.web_port.to_string(),
        "-j",
        "RETURN",
    ]);
    input_chain.rules.extend(input_rules);

    let mut forward_chain = ChainSpec::new("filter", FORWARD_CHAIN);
    forward_chain.push(&[
        "-m",
        "conntrack",
        "--ctstate",
        "ESTABLISHED,RELATED",
        "-j",
        "RETURN",
    ]);
    forward_chain.rules.extend(forward_rules);

    NetAccessPlan {
        plan: IptablesPlan {
            chains: vec![input_chain, forward_chain],
            jumps: vec![
                Jump::new("filter", &["INPUT"], INPUT_CHAIN, true)
                    .before(crate::egress::CONTAINER_INPUT_CHAIN),
                Jump::new("filter", &["DOCKER-USER", "FORWARD"], FORWARD_CHAIN, true),
            ],
        },
        warnings,
    }
}

fn overlaps(a: Ipv4Net, b: Ipv4Net) -> bool {
    a.contains(&b) || b.contains(&a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iptables::jump_commands;

    fn net(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }

    fn denied(name: &str, subnet: &str, addr: Option<&str>) -> Denied {
        Denied {
            network: name.into(),
            subnet: net(subnet),
            node_addr: addr.map(|a| a.parse().unwrap()),
        }
    }

    fn input(sources: Vec<SourceRules>) -> NetAccessInput {
        NetAccessInput {
            web_port: 8443,
            sources,
        }
    }

    fn lan_denied_tailnet() -> NetAccessInput {
        input(vec![SourceRules {
            network: "lan".into(),
            subnet: net("192.168.1.0/24"),
            denied: vec![denied("tailnet", "100.64.0.0/10", Some("100.64.0.1"))],
        }])
    }

    fn is_drop(r: &str) -> bool {
        r.ends_with("DROP")
    }

    #[test]
    fn established_comes_first_and_returns() {
        let plan = net_access_plan(&lan_denied_tailnet()).plan;
        for name in [INPUT_CHAIN, FORWARD_CHAIN] {
            assert_eq!(
                plan.chain(name).unwrap().rules[0],
                "-m conntrack --ctstate ESTABLISHED,RELATED -j RETURN"
            );
        }
    }

    #[test]
    fn the_web_port_is_exempt_on_input_only() {
        let plan = net_access_plan(&lan_denied_tailnet()).plan;
        let input = plan.chain(INPUT_CHAIN).unwrap();
        let exempt = input
            .rules
            .iter()
            .position(|r| r == "-p tcp --dport 8443 -j RETURN")
            .unwrap();
        let first_drop = input.rules.iter().position(|r| is_drop(r)).unwrap();
        assert!(exempt < first_drop);
        assert!(!plan
            .chain(FORWARD_CHAIN)
            .unwrap()
            .rules
            .iter()
            .any(|r| r.contains("8443")));
    }

    #[test]
    fn local_traffic_on_lo_is_never_dropped() {
        let plan = net_access_plan(&lan_denied_tailnet()).plan;
        let rules = &plan.chain(INPUT_CHAIN).unwrap().rules;
        let lo = rules.iter().position(|r| r == "-i lo -j RETURN").unwrap();
        let first_drop = rules.iter().position(|r| is_drop(r)).unwrap();
        assert!(lo < first_drop);
    }

    #[test]
    fn deny_pairs_are_dropped_on_input_and_forward() {
        let plan = net_access_plan(&lan_denied_tailnet()).plan;
        assert!(plan
            .chain(INPUT_CHAIN)
            .unwrap()
            .rules
            .contains(&"-s 192.168.1.0/24 -d 100.64.0.1/32 -j DROP".to_string()));
        assert!(plan
            .chain(FORWARD_CHAIN)
            .unwrap()
            .rules
            .contains(&"-s 192.168.1.0/24 -d 100.64.0.0/10 -j DROP".to_string()));
    }

    #[test]
    fn a_node_without_an_address_on_the_target_only_gets_the_forward_drop() {
        let mut i = lan_denied_tailnet();
        i.sources[0].denied[0].node_addr = None;
        let plan = net_access_plan(&i).plan;
        assert!(!plan
            .chain(INPUT_CHAIN)
            .unwrap()
            .rules
            .iter()
            .any(|r| is_drop(r)));
        assert!(plan
            .chain(FORWARD_CHAIN)
            .unwrap()
            .rules
            .iter()
            .any(|r| is_drop(r)));
    }

    #[test]
    fn an_allowed_pair_has_no_rule() {
        // lan may reach tailnet, so the caller lists no denial for it.
        let plan = net_access_plan(&input(vec![SourceRules {
            network: "lan".into(),
            subnet: net("192.168.1.0/24"),
            denied: vec![denied("other", "10.9.0.0/16", Some("10.9.0.1"))],
        }]))
        .plan;
        let all = plan
            .chains
            .iter()
            .flat_map(|c| c.rules.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!all.contains("100.64."));
    }

    #[test]
    fn overlapping_subnets_are_skipped_with_a_warning() {
        let p = net_access_plan(&input(vec![SourceRules {
            network: "a".into(),
            subnet: net("10.0.0.0/16"),
            denied: vec![denied("b", "10.0.5.0/24", Some("10.0.5.1"))],
        }]));
        assert!(p.plan.chains.is_empty() && p.plan.jumps.is_empty());
        assert_eq!(p.warnings.len(), 1);
        assert!(p.warnings[0].contains("a -> b"));
    }

    #[test]
    fn no_restricted_source_or_no_denial_is_an_empty_plan() {
        let p = net_access_plan(&input(vec![]));
        assert_eq!(p, NetAccessPlan::default());
        let p = net_access_plan(&input(vec![SourceRules {
            network: "lan".into(),
            subnet: net("192.168.1.0/24"),
            denied: vec![],
        }]));
        assert_eq!(p, NetAccessPlan::default());
    }

    #[test]
    fn the_input_jump_sits_before_cldr_in_and_forward_hooks_docker_user() {
        let plan = net_access_plan(&lan_denied_tailnet()).plan;
        let jump = &plan.jumps[0];
        assert_eq!(jump.before.as_deref(), Some("CLDR-IN"));
        let listing = |parent: &str| {
            (parent == "INPUT").then(|| "-A INPUT -j CLDR-IN\n-A INPUT -j CLDR-NET\n".to_string())
        };
        let cmds = jump_commands(jump, &listing);
        assert_eq!(
            cmds,
            vec![
                vec!["-D", "INPUT", "-j", "CLDR-NET"],
                vec!["-I", "INPUT", "1", "-j", "CLDR-NET"]
            ]
        );
        assert_eq!(plan.jumps[1].parents, vec!["DOCKER-USER", "FORWARD"]);
        assert!(plan.jumps[1].first);
    }
}
