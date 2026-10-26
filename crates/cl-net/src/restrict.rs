//! Per-stack traffic restriction on the overlay, enforced entirely by the
//! host firewall.
//!
//! A stack that declares `allowed` on its overlay network (see
//! `cl_config::AllowedTarget`) is a [`Restriction`]: everything its containers
//! send is dropped except traffic the stack is entitled to. Three chains
//! carry that, all described as data ([`IptablesPlan`]) and unit tested
//! without root:
//!
//! * [`FORWARD_CHAIN`] (`filter`, first in `DOCKER-USER`): packets routed
//!   through the host. The stack's own subnets, replies and `host:port`
//!   grants return to the normal rules; the rest is dropped.
//! * `CLDR-IN` rules (see `egress::container_input_plan`): packets addressed
//!   to the host itself. Only DNS, the proxy and ICMP echo, plus explicit
//!   grants on ports clouduler already publishes.
//! * [`REDIRECT_CHAIN`] (`nat`, first in `PREROUTING`): proxy traffic to
//!   another node is redirected to this node's own proxy. The proxy tells
//!   stacks apart by source address, and traffic to another node's proxy is
//!   masqueraded to this node's address, which would make every restricted
//!   stack on it indistinguishable from its neighbours.
//!
//! Nothing here ever opens something the unrestricted rules keep closed: a
//! grant can only let through what routing and `CLDR-IN` would carry anyway.
//! Per packet the kernel evaluates a short rule list and clouduler does
//! nothing at all, and with no restricted stack none of these chains exist.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use cl_config::{PortRange, Protocol};
use ipnet::Ipv4Net;

use crate::iptables::{ChainSpec, IptablesPlan, Jump};

/// The `filter` chain [`forward_plan`] fills.
pub const FORWARD_CHAIN: &str = "CLDR-ALLOW";

/// The `nat` chain [`redirect_plan`] fills.
pub const REDIRECT_CHAIN: &str = "CLDR-ALLOW-NAT";

/// One destination a restricted stack may reach by address and port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PortGrant {
    pub protocol: Protocol,
    pub addr: Ipv4Addr,
    pub ports: PortRange,
}

impl PortGrant {
    fn matches(&self, protocol: Protocol, port: u16) -> bool {
        self.protocol == protocol && (self.ports.start..=self.ports.end).contains(&port)
    }

    fn dport(&self) -> String {
        if self.ports.start == self.ports.end {
            self.ports.start.to_string()
        } else {
            format!("{}:{}", self.ports.start, self.ports.end)
        }
    }
}

/// A stack whose traffic is restricted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restriction {
    /// The overlay subnet its containers sit on.
    pub subnet: Ipv4Net,
    /// Every overlay subnet of the same stack. Traffic inside the stack is
    /// always allowed, on any port.
    pub stack_subnets: Vec<Ipv4Net>,
    pub grants: Vec<PortGrant>,
}

impl Restriction {
    fn sorted_grants(&self) -> BTreeSet<PortGrant> {
        self.grants.iter().copied().collect()
    }

    fn sorted_stack_subnets(&self) -> BTreeSet<Ipv4Net> {
        let mut set: BTreeSet<Ipv4Net> = self.stack_subnets.iter().copied().collect();
        set.insert(self.subnet);
        set
    }
}

/// Restrictions in a stable order with the same subnet merged, so the same
/// input always yields the same rules (the plan is re-applied every tick).
fn normalized(restrictions: &[Restriction]) -> Vec<Restriction> {
    let mut sorted: Vec<&Restriction> = restrictions.iter().collect();
    sorted.sort_by_key(|r| r.subnet);
    let mut out: Vec<Restriction> = Vec::new();
    for r in sorted {
        match out.last_mut() {
            Some(last) if last.subnet == r.subnet => {
                last.grants.extend(r.grants.iter().copied());
                last.stack_subnets.extend(r.stack_subnets.iter().copied());
            }
            _ => out.push(r.clone()),
        }
    }
    for r in &mut out {
        r.grants = r.sorted_grants().into_iter().collect();
        r.stack_subnets = r.sorted_stack_subnets().into_iter().collect();
    }
    out
}

/// The routed-traffic filter. Empty `restrictions` yields no plan: the caller
/// removes the chain instead of leaving a jump that does nothing.
pub fn forward_plan(restrictions: &[Restriction]) -> IptablesPlan {
    let mut chain = ChainSpec::new("filter", FORWARD_CHAIN);
    for r in normalized(restrictions) {
        let src = r.subnet.to_string();
        // RETURN, not ACCEPT: the packet goes on through the rules behind
        // this chain (docker's, the egress kill-switch), which still apply.
        chain.push(&[
            "-s",
            &src,
            "-m",
            "conntrack",
            "--ctstate",
            "ESTABLISHED,RELATED",
            "-j",
            "RETURN",
        ]);
        for dst in &r.stack_subnets {
            chain.push(&["-s", &src, "-d", &dst.to_string(), "-j", "RETURN"]);
        }
        for g in &r.grants {
            chain.push(&[
                "-s",
                &src,
                "-d",
                &format!("{}/32", g.addr),
                "-p",
                &g.protocol.to_string(),
                "--dport",
                &g.dport(),
                "-j",
                "RETURN",
            ]);
        }
        chain.push(&["-s", &src, "-j", "DROP"]);
    }
    IptablesPlan {
        chains: vec![chain],
        jumps: vec![Jump::new(
            "filter",
            &["DOCKER-USER", "FORWARD"],
            FORWARD_CHAIN,
            true,
        )],
    }
}

/// Sends a restricted stack's proxy traffic to another node to this node's own
/// proxy (`local`) instead, so the proxy sees the container's own address.
///
/// There is deliberately no exemption for a grant on a node's proxy port: that
/// traffic would be masqueraded to the node's address, the proxy could no
/// longer tell the stack apart, and its host list (and the network gate) would
/// stop applying. Such a grant is carried by the local proxy like any other
/// proxy traffic.
pub fn redirect_plan(
    restrictions: &[Restriction],
    other_nodes: &[Ipv4Addr],
    local: Ipv4Addr,
    proxy_ports: &[u16],
) -> IptablesPlan {
    let mut chain = ChainSpec::new("nat", REDIRECT_CHAIN);
    let ports: BTreeSet<u16> = proxy_ports.iter().copied().collect();
    let others: BTreeSet<Ipv4Addr> = other_nodes
        .iter()
        .copied()
        .filter(|n| *n != local)
        .collect();
    if !ports.is_empty() {
        let list = ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let local = local.to_string();
        for r in normalized(restrictions) {
            let src = r.subnet.to_string();
            for node in &others {
                chain.push(&[
                    "-s",
                    &src,
                    "-d",
                    &format!("{node}/32"),
                    "-p",
                    "tcp",
                    "-m",
                    "multiport",
                    "--dports",
                    &list,
                    "-j",
                    "DNAT",
                    "--to-destination",
                    &local,
                ]);
            }
        }
    }
    IptablesPlan {
        chains: vec![chain],
        jumps: vec![Jump::new("nat", &["PREROUTING"], REDIRECT_CHAIN, true)],
    }
}

/// What a restricted stack may send to the host itself, as the rules
/// `container_input_plan` places ahead of its pool-wide ones. The final rule
/// drops everything else from the stack.
///
/// A grant opens a host port only when `published` (the ports clouduler
/// itself forwards on every node) already lists it, so a stack file cannot
/// talk its way past the host's own input filter.
pub(crate) fn input_rules(
    restrictions: &[Restriction],
    overlay_pool: Ipv4Net,
    proxy_ports: &[u16],
    published: &[(Protocol, u16)],
) -> Vec<Vec<String>> {
    let mut rules = Vec::new();
    let overlay = overlay_pool.to_string();
    let proxy: BTreeSet<u16> = proxy_ports.iter().copied().collect();
    let published: BTreeSet<(Protocol, u16)> = published.iter().copied().collect();
    let owned = |parts: &[&str]| parts.iter().map(|p| p.to_string()).collect::<Vec<_>>();

    for r in normalized(restrictions) {
        let src = r.subnet.to_string();
        for proto in ["udp", "tcp"] {
            rules.push(owned(&[
                "-s", &src, "-d", &overlay, "-p", proto, "--dport", "53", "-j", "ACCEPT",
            ]));
        }
        if !proxy.is_empty() {
            let list = proxy
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(",");
            rules.push(owned(&[
                "-s",
                &src,
                "-p",
                "tcp",
                "-m",
                "multiport",
                "--dports",
                &list,
                "-j",
                "ACCEPT",
            ]));
        }
        for g in &r.grants {
            for (proto, port) in published.iter().filter(|(p, n)| g.matches(*p, *n)) {
                rules.push(owned(&[
                    "-s",
                    &src,
                    "-d",
                    &format!("{}/32", g.addr),
                    "-p",
                    &proto.to_string(),
                    "--dport",
                    &port.to_string(),
                    "-j",
                    "ACCEPT",
                ]));
            }
        }
        rules.push(owned(&[
            "-s",
            &src,
            "-p",
            "icmp",
            "--icmp-type",
            "echo-request",
            "-j",
            "ACCEPT",
        ]));
        rules.push(owned(&["-s", &src, "-j", "DROP"]));
    }
    rules
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::{container_input_plan, container_pools, overlay_pool, HostServices};

    fn net(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }
    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }
    fn grant(proto: Protocol, addr: &str, lo: u16, hi: u16) -> PortGrant {
        PortGrant {
            protocol: proto,
            addr: ip(addr),
            ports: PortRange { start: lo, end: hi },
        }
    }

    // ---- a tiny evaluator for the rule subset these plans emit ----

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Verdict {
        Accept,
        Drop,
        Return,
        Dnat(Ipv4Addr),
        /// Fell off the end of the chain.
        Fallthrough,
    }

    #[derive(Debug, Clone, Copy)]
    struct Packet {
        src: Ipv4Addr,
        dst: Ipv4Addr,
        /// `None` is ICMP echo request.
        proto: Option<Protocol>,
        dport: u16,
        established: bool,
    }

    fn pkt(src: &str, dst: &str, proto: Protocol, dport: u16) -> Packet {
        Packet {
            src: ip(src),
            dst: ip(dst),
            proto: Some(proto),
            dport,
            established: false,
        }
    }

    fn ping(src: &str, dst: &str) -> Packet {
        Packet {
            src: ip(src),
            dst: ip(dst),
            proto: None,
            dport: 0,
            established: false,
        }
    }

    fn established(mut p: Packet) -> Packet {
        p.established = true;
        p
    }

    fn rule_matches(rule: &str, p: &Packet) -> Option<Verdict> {
        let t: Vec<&str> = rule.split_whitespace().collect();
        let arg = |flag: &str| t.iter().position(|x| *x == flag).map(|i| t[i + 1]);
        if let Some(s) = arg("-s") {
            if !s.parse::<Ipv4Net>().unwrap().contains(&p.src) {
                return None;
            }
        }
        if let Some(d) = arg("-d") {
            if !d.parse::<Ipv4Net>().unwrap().contains(&p.dst) {
                return None;
            }
        }
        if let Some(proto) = arg("-p") {
            match (proto, p.proto) {
                ("tcp", Some(Protocol::Tcp)) | ("udp", Some(Protocol::Udp)) => {}
                ("icmp", None) => {}
                _ => return None,
            }
        }
        if let Some(d) = arg("--dport") {
            let hit = match d.split_once(':') {
                Some((a, b)) => (a.parse().unwrap()..=b.parse().unwrap()).contains(&p.dport),
                None => d.parse::<u16>().unwrap() == p.dport,
            };
            if !hit {
                return None;
            }
        }
        if let Some(list) = arg("--dports") {
            if !list
                .split(',')
                .any(|x| x.parse::<u16>().unwrap() == p.dport)
            {
                return None;
            }
        }
        if arg("--ctstate").is_some() && !p.established {
            return None;
        }
        if arg("--icmp-type").is_some() && p.proto.is_some() {
            return None;
        }
        Some(match arg("-j").unwrap() {
            "ACCEPT" => Verdict::Accept,
            "DROP" => Verdict::Drop,
            "RETURN" => Verdict::Return,
            "DNAT" => Verdict::Dnat(ip(arg("--to-destination").unwrap())),
            other => panic!("evaluator does not know -j {other}"),
        })
    }

    fn eval(rules: &[String], p: &Packet) -> Verdict {
        for rule in rules {
            if let Some(v) = rule_matches(rule, p) {
                return v;
            }
        }
        Verdict::Fallthrough
    }

    fn chain_rules(plan: &IptablesPlan, name: &str) -> Vec<String> {
        plan.chain(name).expect("chain").rules.clone()
    }

    // The scenario used throughout: stack `blog` on 10.244.1.0/24 (and a second
    // overlay 10.244.9.0/24), stack `wiki` on 10.244.2.0/24 unrestricted.
    fn blog() -> Restriction {
        Restriction {
            subnet: net("10.244.1.0/24"),
            stack_subnets: vec![net("10.244.1.0/24"), net("10.244.9.0/24")],
            grants: vec![
                grant(Protocol::Tcp, "100.64.0.9", 22, 22),
                grant(Protocol::Udp, "100.64.0.9", 5000, 5100),
            ],
        }
    }

    fn forward() -> Vec<String> {
        chain_rules(&forward_plan(&[blog()]), FORWARD_CHAIN)
    }

    #[test]
    fn traffic_inside_the_stack_is_always_allowed_on_any_port() {
        let rules = forward();
        for port in [1, 22, 5432, 65535] {
            for dst in ["10.244.1.7", "10.244.9.3"] {
                assert_eq!(
                    eval(&rules, &pkt("10.244.1.5", dst, Protocol::Tcp, port)),
                    Verdict::Return
                );
                assert_eq!(
                    eval(&rules, &pkt("10.244.1.5", dst, Protocol::Udp, port)),
                    Verdict::Return
                );
            }
        }
        assert_eq!(
            eval(&rules, &ping("10.244.1.5", "10.244.1.7")),
            Verdict::Return
        );
    }

    #[test]
    fn another_stack_is_unreachable() {
        let rules = forward();
        assert_eq!(
            eval(&rules, &pkt("10.244.1.5", "10.244.2.5", Protocol::Tcp, 80)),
            Verdict::Drop
        );
        assert_eq!(
            eval(&rules, &ping("10.244.1.5", "10.244.2.5")),
            Verdict::Drop
        );
    }

    #[test]
    fn a_grant_opens_exactly_its_address_protocol_and_ports() {
        let rules = forward();
        let ok = |dst, proto, port| {
            eval(&rules, &pkt("10.244.1.5", dst, proto, port)) == Verdict::Return
        };
        assert!(ok("100.64.0.9", Protocol::Tcp, 22));
        assert!(ok("100.64.0.9", Protocol::Udp, 5000));
        assert!(ok("100.64.0.9", Protocol::Udp, 5100));
        assert!(ok("100.64.0.9", Protocol::Udp, 5050));
        // One port, one protocol, one address off in each direction.
        assert!(!ok("100.64.0.9", Protocol::Udp, 22));
        assert!(!ok("100.64.0.9", Protocol::Tcp, 23));
        assert!(!ok("100.64.0.9", Protocol::Tcp, 5000));
        assert!(!ok("100.64.0.9", Protocol::Udp, 4999));
        assert!(!ok("100.64.0.9", Protocol::Udp, 5101));
        assert!(!ok("100.64.0.10", Protocol::Tcp, 22));
        assert!(!ok("100.64.0.8", Protocol::Tcp, 22));
    }

    #[test]
    fn replies_to_connections_the_stack_started_pass() {
        let rules = forward();
        let reply = established(pkt("10.244.1.5", "100.64.0.77", Protocol::Tcp, 51000));
        assert_eq!(eval(&rules, &reply), Verdict::Return);
        let fresh = pkt("10.244.1.5", "100.64.0.77", Protocol::Tcp, 51000);
        assert_eq!(eval(&rules, &fresh), Verdict::Drop);
    }

    #[test]
    fn other_sources_are_untouched() {
        let rules = forward();
        for src in ["10.244.2.5", "10.245.3.4", "192.168.1.1", "10.244.1.0"] {
            let v = eval(&rules, &pkt(src, "8.8.8.8", Protocol::Tcp, 443));
            if src == "10.244.1.0" {
                // The network address itself is inside the subnet.
                assert_eq!(v, Verdict::Drop);
            } else {
                assert_eq!(v, Verdict::Fallthrough, "{src}");
            }
        }
        // Traffic *to* the stack is not this chain's business.
        assert_eq!(
            eval(&rules, &pkt("10.244.2.5", "10.244.1.5", Protocol::Tcp, 80)),
            Verdict::Fallthrough
        );
    }

    #[test]
    fn the_filter_never_accepts_so_later_rules_still_apply() {
        for rule in forward() {
            assert!(!rule.ends_with("ACCEPT"), "{rule}");
        }
    }

    #[test]
    fn every_restricted_source_ends_in_a_drop() {
        let a = blog();
        let b = Restriction {
            subnet: net("10.244.5.0/24"),
            stack_subnets: vec![],
            grants: vec![],
        };
        let rules = chain_rules(&forward_plan(&[a, b]), FORWARD_CHAIN);
        assert_eq!(
            rules.iter().filter(|r| r.ends_with("-j DROP")).count(),
            2,
            "{rules:#?}"
        );
        // A stack with no grants and no listed subnets still reaches itself.
        assert_eq!(
            eval(&rules, &pkt("10.244.5.2", "10.244.5.3", Protocol::Tcp, 1)),
            Verdict::Return
        );
        assert_eq!(
            eval(&rules, &pkt("10.244.5.2", "10.244.1.3", Protocol::Tcp, 1)),
            Verdict::Drop
        );
    }

    #[test]
    fn plans_are_deterministic_and_deduplicated() {
        let a = blog();
        let mut shuffled = blog();
        shuffled.grants.reverse();
        shuffled.grants.push(shuffled.grants[0]);
        shuffled.stack_subnets.reverse();
        let other = Restriction {
            subnet: net("10.244.0.0/24"),
            stack_subnets: vec![],
            grants: vec![],
        };
        assert_eq!(
            forward_plan(&[a.clone(), other.clone()]),
            forward_plan(&[other, shuffled])
        );
    }

    #[test]
    fn the_filter_jumps_in_ahead_of_dockers_rules() {
        let plan = forward_plan(&[blog()]);
        assert_eq!(plan.jumps.len(), 1);
        assert_eq!(plan.jumps[0].parents, vec!["DOCKER-USER", "FORWARD"]);
        assert!(plan.jumps[0].first);
        assert_eq!(plan.jumps[0].table, "filter");
    }

    // ---- proxy redirect ----

    fn redirect() -> Vec<String> {
        let plan = redirect_plan(
            &[blog()],
            &[ip("100.64.0.2"), ip("100.64.0.9"), ip("100.64.0.1")],
            ip("100.64.0.1"),
            &[80, 443],
        );
        assert_eq!(plan.jumps[0].parents, vec!["PREROUTING"]);
        assert!(plan.jumps[0].first);
        chain_rules(&plan, REDIRECT_CHAIN)
    }

    #[test]
    fn proxy_traffic_to_another_node_lands_on_the_local_proxy() {
        let rules = redirect();
        for port in [80, 443] {
            assert_eq!(
                eval(
                    &rules,
                    &pkt("10.244.1.5", "100.64.0.2", Protocol::Tcp, port)
                ),
                Verdict::Dnat(ip("100.64.0.1"))
            );
        }
    }

    #[test]
    fn only_restricted_stacks_proxy_traffic_to_other_nodes_is_redirected() {
        let rules = redirect();
        // Another port, UDP, the local node itself and another stack.
        assert_eq!(
            eval(
                &rules,
                &pkt("10.244.1.5", "100.64.0.2", Protocol::Tcp, 8080)
            ),
            Verdict::Fallthrough
        );
        assert_eq!(
            eval(&rules, &pkt("10.244.1.5", "100.64.0.2", Protocol::Udp, 443)),
            Verdict::Fallthrough
        );
        assert_eq!(
            eval(&rules, &pkt("10.244.1.5", "100.64.0.1", Protocol::Tcp, 443)),
            Verdict::Fallthrough
        );
        assert_eq!(
            eval(&rules, &pkt("10.244.2.5", "100.64.0.2", Protocol::Tcp, 443)),
            Verdict::Fallthrough
        );
    }

    #[test]
    fn a_grant_on_a_nodes_proxy_port_is_still_redirected_so_the_host_list_applies() {
        let mut r = blog();
        r.grants.push(grant(Protocol::Tcp, "100.64.0.2", 443, 443));
        let plan = redirect_plan(&[r], &[ip("100.64.0.2")], ip("100.64.0.1"), &[80, 443]);
        let rules = chain_rules(&plan, REDIRECT_CHAIN);
        assert_eq!(
            eval(&rules, &pkt("10.244.1.5", "100.64.0.2", Protocol::Tcp, 443)),
            Verdict::Dnat(ip("100.64.0.1"))
        );
    }

    #[test]
    fn no_proxy_means_nothing_to_redirect() {
        let plan = redirect_plan(&[blog()], &[ip("100.64.0.2")], ip("100.64.0.1"), &[]);
        assert!(chain_rules(&plan, REDIRECT_CHAIN).is_empty());
    }

    // ---- the host input filter ----

    fn input_chain(restrictions: Vec<Restriction>) -> Vec<String> {
        let plan = container_input_plan(
            overlay_pool(),
            &container_pools(),
            &HostServices {
                proxy_ports: vec![80, 443],
                published: vec![(Protocol::Tcp, 2222), (Protocol::Udp, 51413)],
                restricted: restrictions,
            },
        );
        chain_rules(&plan, crate::egress::CONTAINER_INPUT_CHAIN)
    }

    #[test]
    fn a_restricted_stack_reaches_only_dns_the_proxy_and_ping_on_the_host() {
        let rules = input_chain(vec![blog()]);
        let from = |dst: &str, proto, port| eval(&rules, &pkt("10.244.1.5", dst, proto, port));
        assert_eq!(from("10.244.1.1", Protocol::Udp, 53), Verdict::Accept);
        assert_eq!(from("10.244.1.1", Protocol::Tcp, 53), Verdict::Accept);
        assert_eq!(from("100.64.0.1", Protocol::Tcp, 80), Verdict::Accept);
        assert_eq!(from("100.64.0.1", Protocol::Tcp, 443), Verdict::Accept);
        assert_eq!(
            eval(&rules, &ping("10.244.1.5", "100.64.0.1")),
            Verdict::Accept
        );
        // The raft/api style ports, and the published ports without a grant.
        for port in [22, 8443, 7000, 7001, 2222] {
            assert_eq!(
                from("100.64.0.1", Protocol::Tcp, port),
                Verdict::Drop,
                "{port}"
            );
        }
        assert_eq!(from("100.64.0.1", Protocol::Udp, 51413), Verdict::Drop);
    }

    #[test]
    fn a_grant_reopens_only_a_port_the_host_already_publishes() {
        let mut r = blog();
        r.grants = vec![
            grant(Protocol::Tcp, "100.64.0.1", 2000, 3000),
            grant(Protocol::Tcp, "100.64.0.1", 7000, 7001),
        ];
        let rules = input_chain(vec![r]);
        let from = |port, proto| eval(&rules, &pkt("10.244.1.5", "100.64.0.1", proto, port));
        assert_eq!(from(2222, Protocol::Tcp), Verdict::Accept);
        assert_eq!(from(2223, Protocol::Tcp), Verdict::Drop);
        // Granted by the stack but not published by clouduler: stays closed.
        assert_eq!(from(7000, Protocol::Tcp), Verdict::Drop);
        // Published, granted on tcp only.
        assert_eq!(from(51413, Protocol::Udp), Verdict::Drop);
    }

    #[test]
    fn other_containers_keep_the_pool_wide_rules() {
        let rules = input_chain(vec![blog()]);
        let from = |port| {
            eval(
                &rules,
                &pkt("10.244.2.5", "100.64.0.1", Protocol::Tcp, port),
            )
        };
        assert_eq!(from(443), Verdict::Accept);
        assert_eq!(from(2222), Verdict::Accept);
        assert_eq!(from(7000), Verdict::Drop);
    }

    #[test]
    fn replies_stay_first_in_the_input_filter() {
        let rules = input_chain(vec![blog()]);
        assert_eq!(
            eval(
                &rules,
                &established(pkt("10.244.1.5", "100.64.0.1", Protocol::Tcp, 7000))
            ),
            Verdict::Accept
        );
    }

    #[test]
    fn without_restrictions_the_input_filter_is_the_unrestricted_one() {
        let with = input_chain(vec![]);
        let plan = container_input_plan(
            overlay_pool(),
            &container_pools(),
            &HostServices {
                proxy_ports: vec![80, 443],
                published: vec![(Protocol::Tcp, 2222), (Protocol::Udp, 51413)],
                ..HostServices::default()
            },
        );
        assert_eq!(
            with,
            chain_rules(&plan, crate::egress::CONTAINER_INPUT_CHAIN)
        );
    }

    /// The emitted rules, run through the real `iptables-restore --test`. Needs
    /// root and the xtables modules, so it only runs on request.
    #[test]
    #[ignore = "needs root and iptables-restore"]
    fn the_real_iptables_accepts_every_emitted_chain() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let mut r = blog();
        r.grants
            .push(grant(Protocol::Tcp, "100.64.0.1", 2222, 2222));
        let input = container_input_plan(
            overlay_pool(),
            &container_pools(),
            &HostServices {
                proxy_ports: vec![80, 443],
                published: vec![(Protocol::Tcp, 2222)],
                restricted: vec![r.clone()],
            },
        );
        let plans = [
            forward_plan(std::slice::from_ref(&r)),
            redirect_plan(
                std::slice::from_ref(&r),
                &[ip("100.64.0.2")],
                ip("100.64.0.1"),
                &[80, 443],
            ),
            input,
        ];
        for plan in plans {
            let payload = crate::iptables::restore_payload(&plan.chains);
            let mut child = Command::new("iptables-restore")
                .args(["-w", "--test", "--noflush"])
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(payload.as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{}\n{payload}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
