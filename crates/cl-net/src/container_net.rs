//! Container-kind networks: traffic to a routed subnet is sent through a
//! router container that sits on a cluster-wide L2 segment.
//!
//! Like `egress` and `wireguard`, everything here is pure: names, the policy
//! routing plan as data, the iptables chains and the routes pushed into
//! container network namespaces. The daemon's reconcile loop owns applying
//! them. The two applying helpers at the bottom ([`apply_routing`],
//! [`scan_routing`]) shell out to `ip` and need CAP_NET_ADMIN.
//!
//! # Marks and priorities
//!
//! * Connmark/fwmark: [`MARK_BASE`] `|` a 16 bit per-network value, always
//!   matched under [`MARK_MASK`] (bits 0-15 and 24-27). Docker does not mark,
//!   kube-proxy uses 0x4000/0x8000 and tailscale uses 0x40000/0x80000 (mask
//!   0xff0000), so none of those fall under the mask.
//! * `ip rule` priorities: [`FWMARK_PRIORITY`] (5050) for the reply path,
//!   then [`EXEMPT_PRIORITY`] (5200, 5201), [`SUPPRESS_PRIORITY`] (5210) and
//!   [`LOOKUP_PRIORITY`] (5211). All come after egress's 5100, so an egress
//!   bridge keeps winning.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::process::Command;

use cl_config::Protocol;
use ipnet::Ipv4Net;

use crate::egress::{short_name, CHAIN_USABLE, IFNAMSIZ_USABLE};
use crate::error::{NetError, Result};
use crate::iptables::{ChainSpec, IptablesPlan, Jump};

/// Every chain this module creates starts with this prefix; the orphan sweep
/// relies on it (and `CLDR-CTN-` etc. cannot be confused with any other
/// clouduler chain).
pub const CHAIN_PREFIX: &str = "CLDR-CTN";
/// Prefix of the host-side veth. The sweep removes `clch-*` links of networks
/// that are no longer wanted.
pub const VETH_PREFIX: &str = "clch-";
const PEER_PREFIX: &str = "clcp-";

/// `ip rule` priority of the reply-path fwmark rule.
pub const FWMARK_PRIORITY: u32 = 5050;
/// Exemptions: `from <router ip>` (5200) and `iif <router bridge>` (5201),
/// both to the main table.
pub const EXEMPT_PRIORITY: u32 = 5200;
pub const EXEMPT_IIF_PRIORITY: u32 = 5201;
/// `lookup main suppress_prefixlength N` of the network with rank 0. Network
/// `r` uses `SUPPRESS_PRIORITY + 2 * r`, immediately followed by its lookup at
/// `LOOKUP_PRIORITY + 2 * r`, so several networks on one host evaluate as
/// ordered (suppress, lookup) pairs; see [`routing_ranks`].
pub const SUPPRESS_PRIORITY: u32 = 5210;
/// `lookup <dst table>` of the network with rank 0.
pub const LOOKUP_PRIORITY: u32 = 5211;
/// How many container networks get their own priority pair; further ones
/// share the last pair.
pub const MAX_RANKS: u32 = 64;

/// The rank of every container network, given `(name, routed subnet)`.
///
/// Longer prefixes rank first (so a `/24` behind one router wins over a
/// `/10` behind another, and `0.0.0.0/0` is always last), ties break on the
/// name. Every node computes it from the same config, so they agree.
pub fn routing_ranks<'a>(
    networks: impl IntoIterator<Item = (&'a str, Ipv4Net)>,
) -> BTreeMap<String, u32> {
    let mut all: Vec<(&str, Ipv4Net)> = networks.into_iter().collect();
    all.sort_by(|a, b| {
        b.1.prefix_len()
            .cmp(&a.1.prefix_len())
            .then_with(|| a.0.cmp(b.0))
    });
    all.into_iter()
        .enumerate()
        .map(|(i, (name, _))| (name.to_string(), (i as u32).min(MAX_RANKS - 1)))
        .collect()
}

fn is_pair_priority(priority: u32) -> bool {
    (SUPPRESS_PRIORITY..SUPPRESS_PRIORITY + 2 * MAX_RANKS).contains(&priority)
}

/// High bits of every container-network mark (bits 24-27 = 0xC).
pub const MARK_BASE: u32 = 0x0C00_0000;
/// The bits of the packet mark this module owns.
pub const MARK_MASK: u32 = 0x0F00_FFFF;

/// Docker/overlay key of a container network's cluster-wide segment.
///
/// Stack names are lowercase (`validate_stack_name`), so an overlay named
/// `<stack>_<network>` never starts with an uppercase letter; the `CTN_`
/// prefix can therefore never collide with one. It is still a valid docker
/// network name (starts with an alphanumeric).
pub fn container_docker_name(network: &str) -> String {
    format!("CTN_{network}")
}

/// Bridge of the segment (the existing overlay helper, keyed on the docker name).
pub fn container_bridge_name(network: &str) -> String {
    crate::vxlan::bridge_name(&container_docker_name(network))
}

/// VXLAN device of the segment.
pub fn container_vxlan_name(network: &str) -> String {
    crate::vxlan::vxlan_name(&container_docker_name(network))
}

/// The host-side veth carrying this node's `X_ctn` address.
pub fn host_veth_name(network: &str) -> String {
    short_name(VETH_PREFIX, network, IFNAMSIZ_USABLE)
}

/// The veth's bridge-side peer.
pub fn host_peer_name(network: &str) -> String {
    short_name(PEER_PREFIX, network, IFNAMSIZ_USABLE)
}

/// Filter chain for traffic arriving from the router, jumped from `INPUT`.
pub fn input_chain(network: &str) -> String {
    short_name("CLDR-CTN-", network, CHAIN_USABLE)
}
/// Filter chain for forwarded traffic from the router, jumped from `DOCKER-USER`.
pub fn forward_chain(network: &str) -> String {
    short_name("CLDR-CTNF-", network, CHAIN_USABLE)
}
/// Nat `PREROUTING` chain (DNAT of opened ports and the proxy).
pub fn nat_chain(network: &str) -> String {
    short_name("CLDR-CTNN-", network, CHAIN_USABLE)
}
/// Nat `POSTROUTING` chain (masquerade).
pub fn masq_chain(network: &str) -> String {
    short_name("CLDR-CTNP-", network, CHAIN_USABLE)
}
/// Mangle `PREROUTING` chain (connmark set + restore).
pub fn mangle_chain(network: &str) -> String {
    short_name("CLDR-CTNM-", network, CHAIN_USABLE)
}
/// Mangle `OUTPUT` chain (connmark restore).
pub fn mangle_output_chain(network: &str) -> String {
    short_name("CLDR-CTNR-", network, CHAIN_USABLE)
}
/// L2 isolation chain of a segment bridge.
pub fn l2_chain(bridge: &str) -> String {
    short_name("CLDR-CTNL2-", bridge, CHAIN_USABLE)
}

/// Every iptables chain `(table, name)` a wanted network owns.
pub fn all_chains(network: &str) -> Vec<(&'static str, String)> {
    vec![
        ("filter", input_chain(network)),
        ("filter", forward_chain(network)),
        ("filter", l2_chain(&container_bridge_name(network))),
        ("nat", nat_chain(network)),
        ("nat", masq_chain(network)),
        ("mangle", mangle_chain(network)),
        ("mangle", mangle_output_chain(network)),
    ]
}

/// The connmark/fwmark of a network: [`MARK_BASE`] plus a deterministic
/// non-zero 16 bit value (`cl_config::container_mark`). Config validation
/// refuses two networks that derive the same one.
pub fn container_mark(network: &str) -> u32 {
    cl_config::container_mark(network)
}

/// The `(reply, dst)` routing tables of a network.
pub fn container_tables(network: &str) -> (u32, u32) {
    cl_config::container_table_ids(network)
}

/// MAC of a node's host port on a segment: locally administered unicast
/// (`0x0a` prefix, so it can never equal `gateway_mac`'s `0x02` one), unique
/// per `(network, node)` so a router can reach a specific remote node over
/// VXLAN.
pub fn host_port_mac(network: &str, node: &str) -> [u8; 6] {
    let hash = cl_config::fnv1a(format!("{network}/{node}").as_bytes()).to_be_bytes();
    [0x0a, hash[1], hash[2], hash[3], hash[4], hash[5]]
}

// ---------------------------------------------------------------------------
// Policy routing as data
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuleSelector {
    /// `from all`
    All,
    From(Ipv4Net),
    Iif(String),
    Fwmark {
        mark: u32,
        mask: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuleAction {
    Lookup(u32),
    /// `lookup main`
    LookupMain,
    /// `lookup main suppress_prefixlength N`
    LookupMainSuppress(u8),
}

/// One `ip rule`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct IpRule {
    pub priority: u32,
    pub selector: RuleSelector,
    pub action: RuleAction,
}

impl IpRule {
    /// Args after `ip rule <add|del>` that name exactly this rule.
    pub fn spec_args(&self) -> Vec<String> {
        let mut a = vec!["priority".to_string(), self.priority.to_string()];
        match &self.selector {
            RuleSelector::All => {}
            RuleSelector::From(net) => {
                a.push("from".into());
                a.push(net.to_string());
            }
            RuleSelector::Iif(i) => {
                a.push("iif".into());
                a.push(i.clone());
            }
            RuleSelector::Fwmark { mark, mask } => {
                a.push("fwmark".into());
                a.push(format!("{mark:#x}/{mask:#x}"));
            }
        }
        a.push("lookup".into());
        match &self.action {
            RuleAction::Lookup(t) => a.push(t.to_string()),
            RuleAction::LookupMain => a.push("main".into()),
            RuleAction::LookupMainSuppress(n) => {
                a.push("main".into());
                a.push("suppress_prefixlength".into());
                a.push(n.to_string());
            }
        }
        a
    }

    /// Is this rule one this module installs (see the priority list above)?
    pub fn is_container_rule(&self) -> bool {
        match self.priority {
            FWMARK_PRIORITY => matches!(
                self.selector,
                RuleSelector::Fwmark { mark, mask }
                    if mask == MARK_MASK && mark & 0x0F00_0000 == MARK_BASE
            ),
            EXEMPT_PRIORITY | EXEMPT_IIF_PRIORITY => true,
            p => is_pair_priority(p),
        }
    }
}

/// One route in a numbered table.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TableRoute {
    pub table: u32,
    pub dest: Ipv4Net,
    pub via: Ipv4Addr,
    pub dev: String,
    pub src: Option<Ipv4Addr>,
    pub onlink: bool,
}

impl TableRoute {
    fn dest_arg(&self) -> String {
        dest_arg(self.dest)
    }

    /// Args for `ip route replace`.
    pub fn replace_args(&self) -> Vec<String> {
        let mut a = vec![
            "route".to_string(),
            "replace".into(),
            self.dest_arg(),
            "via".into(),
            self.via.to_string(),
            "dev".into(),
            self.dev.clone(),
        ];
        if let Some(src) = self.src {
            a.push("src".into());
            a.push(src.to_string());
        }
        if self.onlink {
            a.push("onlink".into());
        }
        a.push("table".into());
        a.push(self.table.to_string());
        a
    }

    /// Args for `ip route del`.
    pub fn del_args(&self) -> Vec<String> {
        vec![
            "route".into(),
            "del".into(),
            self.dest_arg(),
            "table".into(),
            self.table.to_string(),
        ]
    }
}

fn dest_arg(net: Ipv4Net) -> String {
    if net.prefix_len() == 0 {
        "default".to_string()
    } else {
        net.to_string()
    }
}

/// What a node wants for its policy routing: rules, routes, and the tables
/// it manages (even when a table currently has no route, so stale routes in
/// it get removed).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingPlan {
    pub rules: Vec<IpRule>,
    pub routes: Vec<TableRoute>,
    pub tables: BTreeSet<u32>,
}

impl RoutingPlan {
    /// Merge another network's plan into this one.
    pub fn extend(&mut self, other: RoutingPlan) {
        for rule in other.rules {
            if !self.rules.contains(&rule) {
                self.rules.push(rule);
            }
        }
        for route in other.routes {
            if !self.routes.contains(&route) {
                self.routes.push(route);
            }
        }
        self.tables.extend(other.tables);
    }
}

/// Everything [`node_routing_plan`] needs for one container network.
#[derive(Debug, Clone)]
pub struct RoutingInput {
    pub network: String,
    /// The routed address space (`container.subnet`).
    pub subnet: Ipv4Net,
    /// The router's address on the segment; `None` while it has none, which
    /// yields an empty plan (so every rule and route is removed).
    pub router_ip: Option<Ipv4Addr>,
    /// Bridge of the router stack's own overlay network (exempt from our rules).
    pub router_bridge: Option<String>,
    /// This node's `X_ctn`.
    pub node_ctn: Ipv4Addr,
    /// Position among the container networks ([`routing_ranks`]); picks the
    /// suppress/lookup priority pair.
    pub rank: u32,
    /// Overlay subnets of stacks that sit on an allowed network.
    pub stack_sources: Vec<Ipv4Net>,
    /// This node is in `hosts`.
    pub host_listed: bool,
    /// The router container is running (fail-open for hosts otherwise).
    pub router_running: bool,
}

/// The fwmark rule, tables and per-source / host rules of one network on
/// this node. Pure.
pub fn node_routing_plan(input: &RoutingInput) -> RoutingPlan {
    let (reply, dst) = container_tables(&input.network);
    let mut plan = RoutingPlan {
        tables: [reply, dst].into(),
        ..RoutingPlan::default()
    };
    let Some(router) = input.router_ip else {
        return plan;
    };
    let veth = host_veth_name(&input.network);

    plan.routes.push(TableRoute {
        table: reply,
        dest: "0.0.0.0/0".parse().expect("default parses"),
        via: router,
        dev: veth.clone(),
        src: None,
        onlink: true,
    });
    plan.routes.push(TableRoute {
        table: dst,
        dest: input.subnet,
        via: router,
        dev: veth,
        src: Some(input.node_ctn),
        onlink: true,
    });
    plan.rules.push(IpRule {
        priority: FWMARK_PRIORITY,
        selector: RuleSelector::Fwmark {
            mark: container_mark(&input.network),
            mask: MARK_MASK,
        },
        action: RuleAction::Lookup(reply),
    });

    let mut sources: Vec<RuleSelector> = input
        .stack_sources
        .iter()
        .map(|s| RuleSelector::From(*s))
        .collect();
    let hosts_active = input.host_listed && input.router_running;
    if hosts_active {
        sources.push(RuleSelector::All);
    }
    if sources.is_empty() {
        return plan;
    }

    // Exemptions come first: the router itself, and traffic entering from the
    // router stack's own overlay, must never be sent back to the router.
    plan.rules.push(IpRule {
        priority: EXEMPT_PRIORITY,
        selector: RuleSelector::From(Ipv4Net::new(router, 32).expect("/32 is valid")),
        action: RuleAction::LookupMain,
    });
    if let Some(bridge) = &input.router_bridge {
        plan.rules.push(IpRule {
            priority: EXEMPT_IIF_PRIORITY,
            selector: RuleSelector::Iif(bridge.clone()),
            action: RuleAction::LookupMain,
        });
    }
    let offset = 2 * input.rank.min(MAX_RANKS - 1);
    for selector in sources {
        plan.rules.push(IpRule {
            priority: SUPPRESS_PRIORITY + offset,
            selector: selector.clone(),
            action: RuleAction::LookupMainSuppress(input.subnet.prefix_len()),
        });
        plan.rules.push(IpRule {
            priority: LOOKUP_PRIORITY + offset,
            selector,
            action: RuleAction::Lookup(dst),
        });
    }
    plan
}

/// What `ip rule show` and `ip route show table N` currently say.
#[derive(Debug, Clone, Default)]
pub struct HostRouting {
    pub rules: Vec<IpRule>,
    pub routes: BTreeMap<u32, Vec<TableRoute>>,
}

/// Parse `ip rule show`. Lines that are not in a shape this module produces
/// (`iif`, `from`, `fwmark` selectors and `lookup`) are skipped.
pub fn parse_ip_rules(output: &str) -> Vec<IpRule> {
    output.lines().filter_map(parse_ip_rule).collect()
}

fn parse_ip_rule(line: &str) -> Option<IpRule> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let priority: u32 = tokens.first()?.strip_suffix(':')?.parse().ok()?;
    let after = |key: &str| {
        tokens
            .iter()
            .position(|t| *t == key)
            .and_then(|i| tokens.get(i + 1))
            .copied()
    };
    let selector = if let Some(iif) = after("iif") {
        RuleSelector::Iif(iif.to_string())
    } else if let Some(fw) = after("fwmark") {
        let (mark, mask) = match fw.split_once('/') {
            Some((m, k)) => (parse_hex(m)?, parse_hex(k)?),
            None => (parse_hex(fw)?, u32::MAX),
        };
        RuleSelector::Fwmark { mark, mask }
    } else {
        match after("from")? {
            "all" => RuleSelector::All,
            f => RuleSelector::From(parse_net(f)?),
        }
    };
    let table = after("lookup")?;
    let action = if table == "main" {
        match after("suppress_prefixlength") {
            Some(n) => RuleAction::LookupMainSuppress(n.parse().ok()?),
            None => RuleAction::LookupMain,
        }
    } else {
        RuleAction::Lookup(table.parse().ok()?)
    };
    Some(IpRule {
        priority,
        selector,
        action,
    })
}

fn parse_hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).ok()
}

fn parse_net(s: &str) -> Option<Ipv4Net> {
    if s == "default" {
        return Some("0.0.0.0/0".parse().expect("default parses"));
    }
    s.parse::<Ipv4Net>().ok().or_else(|| {
        s.parse::<Ipv4Addr>()
            .ok()
            .and_then(|a| Ipv4Net::new(a, 32).ok())
    })
}

/// Parse `ip route show table N` into the routes this module writes
/// (`<dest> via <ip> dev <if> [src <ip>] [onlink]`). Other lines are skipped.
pub fn parse_table_routes(table: u32, output: &str) -> Vec<TableRoute> {
    output
        .lines()
        .filter_map(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            let after = |key: &str| {
                tokens
                    .iter()
                    .position(|t| *t == key)
                    .and_then(|i| tokens.get(i + 1))
                    .copied()
            };
            Some(TableRoute {
                table,
                dest: parse_net(tokens.first()?)?,
                via: after("via")?.parse().ok()?,
                dev: after("dev")?.to_string(),
                src: after("src").and_then(|s| s.parse().ok()),
                onlink: tokens.contains(&"onlink"),
            })
        })
        .collect()
}

/// The `ip` argument vectors that turn `host` into `plan`. Additions first
/// (routes, then rules), removals of stale owned rules and stale routes in
/// managed tables last, so there is never a window with the new state missing.
pub fn diff_routing(plan: &RoutingPlan, host: &HostRouting) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut present_routes: BTreeSet<&TableRoute> = BTreeSet::new();
    for table in &plan.tables {
        if let Some(routes) = host.routes.get(table) {
            present_routes.extend(routes.iter());
        }
    }
    for route in &plan.routes {
        if !present_routes.contains(route) {
            commands.push(route.replace_args());
        }
    }
    for rule in &plan.rules {
        if !host.rules.contains(rule) {
            let mut a = vec!["rule".to_string(), "add".into()];
            a.extend(rule.spec_args());
            commands.push(a);
        }
    }
    for rule in &host.rules {
        if rule.is_container_rule() && !plan.rules.contains(rule) {
            let mut a = vec!["rule".to_string(), "del".into()];
            a.extend(rule.spec_args());
            commands.push(a);
        }
    }
    for route in present_routes {
        if !plan.routes.contains(route) {
            commands.push(route.del_args());
        }
    }
    commands
}

/// Read the rules and the routes of `tables` from the host.
pub fn scan_routing(tables: &BTreeSet<u32>) -> Result<HostRouting> {
    let rules = run_ip_output(&["rule", "show"])?;
    let mut host = HostRouting {
        rules: parse_ip_rules(&rules),
        routes: BTreeMap::new(),
    };
    for table in tables {
        // A table that was never written reports an error; that is "empty".
        let out =
            run_ip_output(&["route", "show", "table", &table.to_string()]).unwrap_or_default();
        host.routes.insert(*table, parse_table_routes(*table, &out));
    }
    Ok(host)
}

/// Bring the host's rules and tables in line with `plan`. Safe every tick.
/// Every command is tried; the failures come back as one error.
pub fn apply_routing(plan: &RoutingPlan) -> Result<()> {
    let host = scan_routing(&plan.tables)?;
    let mut failures = Vec::new();
    for command in diff_routing(plan, &host) {
        let args: Vec<&str> = command.iter().map(String::as_str).collect();
        if let Err(e) = run_ip(&args) {
            failures.push(e.to_string());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(NetError::Io {
            context: failures.join("; "),
            source: std::io::Error::other("ip failed"),
        })
    }
}

pub(crate) fn run_ip(args: &[&str]) -> Result<()> {
    let out = Command::new("ip")
        .args(args)
        .output()
        .map_err(|e| NetError::Io {
            context: format!("ip {}", args.join(" ")),
            source: e,
        })?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Already gone / already there is the goal, not a failure.
    if stderr.contains("Cannot find device")
        || stderr.contains("No such file")
        || stderr.contains("No such process")
        || stderr.contains("File exists")
    {
        return Ok(());
    }
    Err(NetError::Io {
        context: format!("ip {}: {stderr}", args.join(" ")),
        source: std::io::Error::other("ip exited non-zero"),
    })
}

fn run_ip_output(args: &[&str]) -> Result<String> {
    let out = Command::new("ip")
        .args(args)
        .output()
        .map_err(|e| NetError::Io {
            context: format!("ip {}", args.join(" ")),
            source: e,
        })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(NetError::Io {
            context: format!(
                "ip {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            ),
            source: std::io::Error::other("ip exited non-zero"),
        })
    }
}

// ---------------------------------------------------------------------------
// iptables
// ---------------------------------------------------------------------------

/// How a rule recognizes the router's traffic: on the bridge only packets
/// sourced from the router (clients share that bridge and must not be caught),
/// on the host port everything.
fn ingress_matches(bridge: &str, veth: &str, router_ip: Option<Ipv4Addr>) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    if let Some(router) = router_ip {
        out.push(
            ["-i", bridge, "-s", &format!("{router}/32")]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
    }
    out.push(["-i", veth].iter().map(|s| s.to_string()).collect());
    out
}

fn push_with(chain: &mut ChainSpec, prefix: &[String], rest: &[&str]) {
    let mut parts: Vec<&str> = prefix.iter().map(String::as_str).collect();
    parts.extend_from_slice(rest);
    chain.push(&parts);
}

/// Everything [`filter_iptables_plan`] needs for one container network.
#[derive(Debug, Clone)]
pub struct FilterInput<'a> {
    pub network: &'a str,
    pub bridge: &'a str,
    pub veth: &'a str,
    pub router_ip: Option<Ipv4Addr>,
    /// This node's `X_ctn`.
    pub node_ctn: Ipv4Addr,
    /// `(port, protocol, target_port)`.
    pub open_ports: &'a [(u16, Protocol, u16)],
    /// The node's internal address, the DNAT target of renumbered ports.
    pub target_addr: Option<Ipv4Addr>,
    /// Reverse-proxy ports to open and DNAT to `node_ctn`; empty when
    /// `proxy: false`.
    pub proxy_ports: &'a [u16],
    /// Destinations the router may forward to.
    pub allowed: &'a [Ipv4Net],
}

/// The ingress-from-router rules of one network on one node, as four chains:
///
/// * `input_chain` (filter, from `INPUT`): replies, opened ports, proxy ports,
///   then drop.
/// * `forward_chain` (filter, from `DOCKER-USER`, falling back to `FORWARD`):
///   lets bridged same-segment frames through to the L2 chain, then replies,
///   opened ports (docker `-p` publishes are DNAT-ed and forwarded), proxy
///   ports, forwarding to `allowed`, then drop.
/// * `nat_chain` (nat, from `PREROUTING`): DNAT for renumbered ports and
///   80/443 to `X_ctn` (so the proxy sees a local-subnet peer). Only for
///   `--dst-type LOCAL`: a router forwarding to a LAN host is not captured.
/// * `masq_chain` (nat, from `POSTROUTING`): `-o clch ! -s X_ctn` masquerade
///   (stack/host traffic towards the routed subnet gets `X_ctn` as source)
///   and router-sourced traffic towards `allowed` destinations.
///
/// Ports are matched on the original destination port (`--ctorigdstport`),
/// which survives DNAT, exactly like `wireguard_ingress_iptables_plan`.
///
/// Ordering note for the caller: the `INPUT` jump must be evaluated before
/// `CLDR-IN`, which drops container pools (the segment lives in one).
pub fn filter_iptables_plan(input: &FilterInput<'_>) -> IptablesPlan {
    let matches = ingress_matches(input.bridge, input.veth, input.router_ip);
    let in_name = input_chain(input.network);
    let fwd_name = forward_chain(input.network);
    let nat_name = nat_chain(input.network);
    let masq_name = masq_chain(input.network);
    let mut inp = ChainSpec::new("filter", &in_name);
    let mut fwd = ChainSpec::new("filter", &fwd_name);
    let mut nat = ChainSpec::new("nat", &nat_name);
    let mut masq = ChainSpec::new("nat", &masq_name);

    // Same-segment bridged frames are the L2 chain's business.
    fwd.push(&["-i", input.bridge, "-o", input.bridge, "-j", "RETURN"]);

    for m in &matches {
        for chain in [&mut inp, &mut fwd] {
            push_with(
                chain,
                m,
                &[
                    "-m",
                    "conntrack",
                    "--ctstate",
                    "ESTABLISHED,RELATED",
                    "-j",
                    "ACCEPT",
                ],
            );
        }
    }

    let mut opened: BTreeSet<(String, u16)> = BTreeSet::new();
    let mut accept_port = |inp: &mut ChainSpec, fwd: &mut ChainSpec, proto: &str, port: u16| {
        if !opened.insert((proto.to_string(), port)) {
            return false;
        }
        for m in &matches {
            for chain in [&mut *inp, &mut *fwd] {
                push_with(
                    chain,
                    m,
                    &[
                        "-p",
                        proto,
                        "-m",
                        "conntrack",
                        "--ctorigdstport",
                        &port.to_string(),
                        "-j",
                        "ACCEPT",
                    ],
                );
            }
        }
        true
    };

    for (port, protocol, target) in input.open_ports {
        let proto = protocol.to_string();
        accept_port(&mut inp, &mut fwd, &proto, *port);
        if let (Some(addr), true) = (input.target_addr, target != port) {
            let dest = format!("{addr}:{target}");
            for m in &matches {
                push_with(
                    &mut nat,
                    m,
                    &[
                        "-p",
                        &proto,
                        "--dport",
                        &port.to_string(),
                        "-m",
                        "addrtype",
                        "--dst-type",
                        "LOCAL",
                        "-j",
                        "DNAT",
                        "--to-destination",
                        &dest,
                    ],
                );
            }
        }
    }
    let ctn = input.node_ctn.to_string();
    let ports: BTreeSet<u16> = input.proxy_ports.iter().copied().collect();
    for port in ports {
        accept_port(&mut inp, &mut fwd, "tcp", port);
        for m in &matches {
            push_with(
                &mut nat,
                m,
                &[
                    "-p",
                    "tcp",
                    "--dport",
                    &port.to_string(),
                    "!",
                    "-d",
                    &format!("{ctn}/32"),
                    "-m",
                    "addrtype",
                    "--dst-type",
                    "LOCAL",
                    "-j",
                    "DNAT",
                    "--to-destination",
                    &ctn,
                ],
            );
        }
    }

    let allowed: BTreeSet<Ipv4Net> = input.allowed.iter().copied().collect();
    for net in &allowed {
        for m in &matches {
            push_with(&mut fwd, m, &["-d", &net.to_string(), "-j", "ACCEPT"]);
        }
    }
    for m in &matches {
        for chain in [&mut inp, &mut fwd] {
            push_with(chain, m, &["-j", "DROP"]);
        }
    }

    masq.push(&[
        "-o",
        input.veth,
        "!",
        "-s",
        &format!("{ctn}/32"),
        "-j",
        "MASQUERADE",
    ]);
    if let Some(router) = input.router_ip {
        for net in &allowed {
            masq.push(&[
                "-s",
                &format!("{router}/32"),
                "-d",
                &net.to_string(),
                "-j",
                "MASQUERADE",
            ]);
        }
    }

    IptablesPlan {
        chains: vec![inp, fwd, nat, masq],
        jumps: vec![
            Jump::new("filter", &["INPUT"], &in_name, true)
                .before(crate::egress::CONTAINER_INPUT_CHAIN),
            Jump::new("filter", &["DOCKER-USER", "FORWARD"], &fwd_name, true),
            Jump::new("nat", &["PREROUTING"], &nat_name, false),
            Jump::new("nat", &["POSTROUTING"], &masq_name, false),
        ],
    }
}

/// Connmark rules of one network: NEW connections arriving from the router
/// (on the bridge from its address, or on the host port) get the network's
/// mark; the mark is restored onto every packet of the connection in
/// `PREROUTING` and `OUTPUT`, where `ip rule fwmark` sends it to the reply
/// table.
pub fn mangle_iptables_plan(
    network: &str,
    bridge: &str,
    veth: &str,
    router_ip: Option<Ipv4Addr>,
) -> IptablesPlan {
    let mark = container_mark(network);
    let pre_name = mangle_chain(network);
    let out_name = mangle_output_chain(network);
    let value = format!("{mark:#x}/{MARK_MASK:#x}");
    let mask = format!("{MARK_MASK:#x}");
    let restore = |chain: &mut ChainSpec| {
        chain.push(&[
            "-m",
            "connmark",
            "--mark",
            &value,
            "-j",
            "CONNMARK",
            "--restore-mark",
            "--nfmask",
            &mask,
            "--ctmask",
            &mask,
        ]);
    };
    let mut pre = ChainSpec::new("mangle", &pre_name);
    let mut out = ChainSpec::new("mangle", &out_name);
    restore(&mut pre);
    restore(&mut out);
    for m in ingress_matches(bridge, veth, router_ip) {
        push_with(
            &mut pre,
            &m,
            &[
                "-m",
                "conntrack",
                "--ctstate",
                "NEW",
                "-j",
                "CONNMARK",
                "--set-xmark",
                &value,
            ],
        );
    }
    IptablesPlan {
        chains: vec![pre, out],
        jumps: vec![
            Jump::new("mangle", &["PREROUTING"], &pre_name, true),
            Jump::new("mangle", &["OUTPUT"], &out_name, true),
        ],
    }
}

/// Client-to-client isolation on the shared segment: bridged frames
/// (`-i br -o br`, via br_netfilter) are accepted only when the source or
/// destination is the router or a node address; everything else, i.e. one
/// client talking to another, is dropped.
pub fn l2_isolation_plan(
    bridge: &str,
    router_ip: Option<Ipv4Addr>,
    node_ips: &[Ipv4Addr],
) -> IptablesPlan {
    let name = l2_chain(bridge);
    let mut chain = ChainSpec::new("filter", &name);
    let allowed: BTreeSet<Ipv4Addr> = router_ip
        .into_iter()
        .chain(node_ips.iter().copied())
        .collect();
    for ip in allowed {
        let ip = format!("{ip}/32");
        chain.push(&["-i", bridge, "-o", bridge, "-s", &ip, "-j", "ACCEPT"]);
        chain.push(&["-i", bridge, "-o", bridge, "-d", &ip, "-j", "ACCEPT"]);
    }
    chain.push(&["-i", bridge, "-o", bridge, "-j", "DROP"]);
    IptablesPlan {
        chains: vec![chain],
        jumps: vec![Jump::new(
            "filter",
            &["DOCKER-USER", "FORWARD"],
            &name,
            true,
        )],
    }
}

// ---------------------------------------------------------------------------
// netns routes
// ---------------------------------------------------------------------------

/// `ip route` args (without the leading `ip`) for a client container's
/// namespace: `replace <subnet>|default via <router>`, or `replace prohibit
/// <subnet>` while the router has no address (fail closed).
pub fn client_netns_routes(subnet: Ipv4Net, router_ip: Option<Ipv4Addr>) -> Vec<Vec<String>> {
    let dest = dest_arg(subnet);
    let cmd = match router_ip {
        Some(ip) => vec![
            "route".into(),
            "replace".into(),
            dest,
            "via".into(),
            ip.to_string(),
        ],
        None => vec!["route".into(), "replace".into(), "prohibit".into(), dest],
    };
    vec![cmd]
}

/// Routes for the router container's namespace: each node's addresses on the
/// allowed networks straight to that node's `X_ctn` (VXLAN to the node,
/// source kept), and the allowed subnets via the local node (masqueraded
/// there).
pub fn router_netns_routes(
    node_dests: &[(Ipv4Addr, Ipv4Addr)],
    allowed_subnets: &[Ipv4Net],
    local_node_ctn: Ipv4Addr,
) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for (dest, via) in node_dests.iter().collect::<BTreeSet<_>>() {
        out.push(vec![
            "route".to_string(),
            "replace".into(),
            format!("{dest}/32"),
            "via".into(),
            via.to_string(),
        ]);
    }
    for net in allowed_subnets.iter().collect::<BTreeSet<_>>() {
        out.push(vec![
            "route".to_string(),
            "replace".into(),
            dest_arg(*net),
            "via".into(),
            local_node_ctn.to_string(),
        ]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }
    fn net(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }
    fn rules(plan: &IptablesPlan, chain: &str) -> Vec<String> {
        plan.chain(chain)
            .unwrap_or_else(|| panic!("no chain {chain}"))
            .rules
            .clone()
    }

    #[test]
    fn the_docker_key_cannot_be_a_stack_overlay_name() {
        let key = container_docker_name("tailnet");
        assert_eq!(key, "CTN_tailnet");
        // A valid stack name starts with a lowercase letter or digit.
        assert!(cl_config::validate_stack_name(&key).is_err());
        assert!(!key.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn interface_and_chain_names_fit() {
        for n in [
            "tn",
            "a-very-long-container-network-name-indeed",
            "éééééééééééé",
        ] {
            for name in [
                container_bridge_name(n),
                container_vxlan_name(n),
                host_veth_name(n),
                host_peer_name(n),
            ] {
                assert!(name.len() <= IFNAMSIZ_USABLE, "{name}");
            }
            for (_, chain) in all_chains(n) {
                assert!(chain.len() <= CHAIN_USABLE, "{chain}");
                assert!(chain.starts_with(CHAIN_PREFIX));
            }
        }
        assert_eq!(host_veth_name("tn"), "clch-tn");
        assert_eq!(host_peer_name("tn"), "clcp-tn");
        assert_eq!(input_chain("tn"), "CLDR-CTN-tn");
        assert_eq!(nat_chain("tn"), "CLDR-CTNN-tn");
        assert_eq!(mangle_chain("tn"), "CLDR-CTNM-tn");
        assert_eq!(l2_chain("clbr-x"), "CLDR-CTNL2-clbr-x");
    }

    #[test]
    fn names_of_different_networks_differ_after_truncation() {
        let a = host_veth_name("a-very-long-container-network-one");
        let b = host_veth_name("a-very-long-container-network-two");
        assert_ne!(a, b);
    }

    #[test]
    fn the_mark_is_deterministic_in_range_and_outside_known_marks() {
        let m = container_mark("tn");
        assert_eq!(m, container_mark("tn"));
        assert_eq!(m & 0x0F00_0000, MARK_BASE);
        assert_ne!(m & 0xFFFF, 0);
        // kube-proxy and tailscale marks are outside the mask.
        for foreign in [0x4000u32, 0x8000, 0x40000, 0x80000, 0xca6c] {
            assert_ne!(foreign & MARK_MASK, m & MARK_MASK);
        }
        assert_eq!(0x40000 & MARK_MASK, 0);
        assert_eq!(0x80000 & MARK_MASK, 0);
    }

    #[test]
    fn the_host_port_mac_is_unicast_local_unique_and_not_the_gateway_mac() {
        let m = host_port_mac("tn", "n1");
        assert_eq!(m[0] & 0x01, 0, "unicast");
        assert_eq!(m[0] & 0x02, 0x02, "locally administered");
        assert_eq!(m, host_port_mac("tn", "n1"));
        assert_ne!(m, host_port_mac("tn", "n2"));
        assert_ne!(m, host_port_mac("tn2", "n1"));
        let gw = crate::vxlan::gateway_mac(&container_bridge_name("tn"));
        assert_ne!(m[0], gw[0]);
    }

    fn input() -> RoutingInput {
        RoutingInput {
            network: "tn".into(),
            subnet: net("100.64.0.0/10"),
            router_ip: Some(ip("10.244.9.5")),
            router_bridge: Some("clbr-ts_default".into()),
            node_ctn: ip("10.244.9.2"),
            rank: 0,
            stack_sources: vec![net("10.244.3.0/24")],
            host_listed: true,
            router_running: true,
        }
    }

    #[test]
    fn the_routing_plan_has_reply_and_dst_tables() {
        let plan = node_routing_plan(&input());
        let (reply, dst) = container_tables("tn");
        assert_eq!(plan.tables, [reply, dst].into());
        assert!(plan.routes.contains(&TableRoute {
            table: reply,
            dest: net("0.0.0.0/0"),
            via: ip("10.244.9.5"),
            dev: "clch-tn".into(),
            src: None,
            onlink: true,
        }));
        assert!(plan.routes.contains(&TableRoute {
            table: dst,
            dest: net("100.64.0.0/10"),
            via: ip("10.244.9.5"),
            dev: "clch-tn".into(),
            src: Some(ip("10.244.9.2")),
            onlink: true,
        }));
        assert!(plan.rules.contains(&IpRule {
            priority: 5050,
            selector: RuleSelector::Fwmark {
                mark: container_mark("tn"),
                mask: MARK_MASK
            },
            action: RuleAction::Lookup(reply),
        }));
    }

    #[test]
    fn suppress_then_lookup_for_each_source_and_for_the_host() {
        let plan = node_routing_plan(&input());
        let (_, dst) = container_tables("tn");
        for sel in [RuleSelector::From(net("10.244.3.0/24")), RuleSelector::All] {
            assert!(plan.rules.contains(&IpRule {
                priority: SUPPRESS_PRIORITY,
                selector: sel.clone(),
                action: RuleAction::LookupMainSuppress(10),
            }));
            assert!(plan.rules.contains(&IpRule {
                priority: LOOKUP_PRIORITY,
                selector: sel,
                action: RuleAction::Lookup(dst),
            }));
        }
        // Exemptions sort before everything else of ours.
        let exempt: Vec<_> = plan
            .rules
            .iter()
            .filter(|r| r.action == RuleAction::LookupMain)
            .collect();
        assert_eq!(exempt.len(), 2);
        assert!(exempt.iter().all(|r| r.priority < SUPPRESS_PRIORITY));
        assert!(exempt.iter().all(|r| r.priority > 5100));
    }

    #[test]
    fn ranks_put_longer_prefixes_first_and_default_last() {
        let ranks = routing_ranks([
            ("wide", net("100.64.0.0/10")),
            ("all", net("0.0.0.0/0")),
            ("narrow", net("100.64.5.0/24")),
            ("also-wide", net("172.16.0.0/10")),
        ]);
        assert_eq!(ranks["narrow"], 0);
        assert_eq!(ranks["also-wide"], 1);
        assert_eq!(ranks["wide"], 2);
        assert_eq!(ranks["all"], 3);
    }

    #[test]
    fn two_networks_get_adjacent_suppress_lookup_pairs_in_rank_order() {
        let mut a = input();
        a.rank = 0;
        let mut b = input();
        b.network = "other".into();
        b.subnet = net("0.0.0.0/0");
        b.router_ip = Some(ip("10.244.10.5"));
        b.rank = 1;
        let mut plan = node_routing_plan(&a);
        plan.extend(node_routing_plan(&b));
        let mut pairs: Vec<&IpRule> = plan
            .rules
            .iter()
            .filter(|r| r.selector == RuleSelector::All && r.priority >= SUPPRESS_PRIORITY)
            .collect();
        pairs.sort_by_key(|r| r.priority);
        let prios: Vec<u32> = pairs.iter().map(|r| r.priority).collect();
        assert_eq!(prios, vec![5210, 5211, 5212, 5213]);
        assert!(matches!(
            pairs[0].action,
            RuleAction::LookupMainSuppress(10)
        ));
        assert_eq!(
            pairs[1].action,
            RuleAction::Lookup(container_tables("tn").1)
        );
        assert!(matches!(pairs[2].action, RuleAction::LookupMainSuppress(0)));
        assert_eq!(
            pairs[3].action,
            RuleAction::Lookup(container_tables("other").1)
        );
        assert!(pairs.iter().all(|r| r.is_container_rule()));
        assert!(!IpRule {
            priority: 5210 + 2 * MAX_RANKS,
            selector: RuleSelector::All,
            action: RuleAction::LookupMain
        }
        .is_container_rule());
    }

    #[test]
    fn hosts_fail_open_while_the_router_is_down() {
        let mut i = input();
        i.router_running = false;
        let plan = node_routing_plan(&i);
        assert!(!plan.rules.iter().any(|r| r.selector == RuleSelector::All));
        // Stack sources keep their rules (they fail closed in their netns).
        assert!(plan
            .rules
            .iter()
            .any(|r| r.selector == RuleSelector::From(net("10.244.3.0/24"))));
        i.stack_sources.clear();
        let plan = node_routing_plan(&i);
        // Only the reply path remains.
        assert_eq!(plan.rules.len(), 1);
        assert_eq!(plan.rules[0].priority, FWMARK_PRIORITY);
    }

    #[test]
    fn an_unlisted_host_gets_no_host_rule() {
        let mut i = input();
        i.host_listed = false;
        let plan = node_routing_plan(&i);
        assert!(!plan.rules.iter().any(|r| r.selector == RuleSelector::All));
    }

    #[test]
    fn without_a_router_address_the_plan_is_empty_but_names_its_tables() {
        let mut i = input();
        i.router_ip = None;
        let plan = node_routing_plan(&i);
        assert!(plan.rules.is_empty() && plan.routes.is_empty());
        assert_eq!(plan.tables.len(), 2);
    }

    #[test]
    fn a_default_route_subnet_suppresses_only_default_routes() {
        let mut i = input();
        i.subnet = net("0.0.0.0/0");
        let plan = node_routing_plan(&i);
        assert!(plan
            .rules
            .iter()
            .any(|r| r.action == RuleAction::LookupMainSuppress(0)));
        let args = plan
            .routes
            .iter()
            .find(|r| r.src.is_some())
            .unwrap()
            .replace_args();
        assert_eq!(args[2], "default");
    }

    #[test]
    fn rules_and_routes_round_trip_through_the_ip_output() {
        let text = "0:\tfrom all lookup local\n\
            5050:\tfrom all fwmark 0xc00abcd/0xf00ffff lookup 5123\n\
            5100:\tfrom all iif clex-vpn lookup 5001\n\
            5200:\tfrom 10.244.9.5 lookup main\n\
            5201:\tfrom all iif clbr-ts lookup main\n\
            5210:\tfrom 10.244.3.0/24 lookup main suppress_prefixlength 10\n\
            5211:\tfrom all lookup 5124\n\
            32766:\tfrom all lookup main\n";
        let parsed = parse_ip_rules(text);
        assert!(parsed.contains(&IpRule {
            priority: 5050,
            selector: RuleSelector::Fwmark {
                mark: 0x0c00abcd,
                mask: 0x0f00ffff
            },
            action: RuleAction::Lookup(5123)
        }));
        assert!(parsed.contains(&IpRule {
            priority: 5200,
            selector: RuleSelector::From(net("10.244.9.5/32")),
            action: RuleAction::LookupMain
        }));
        assert!(parsed.contains(&IpRule {
            priority: 5210,
            selector: RuleSelector::From(net("10.244.3.0/24")),
            action: RuleAction::LookupMainSuppress(10)
        }));
        // The egress rule at 5100 and the stock ones are not ours.
        let owned: Vec<_> = parsed.iter().filter(|r| r.is_container_rule()).collect();
        assert_eq!(owned.len(), 5);

        let routes = parse_table_routes(
            5124,
            "100.64.0.0/10 via 10.244.9.5 dev clch-tn src 10.244.9.2 onlink\n\
             default via 10.244.9.5 dev clch-tn onlink\n\
             unreachable 1.2.3.4\n",
        );
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[1].dest, net("0.0.0.0/0"));
        assert_eq!(routes[0].src, Some(ip("10.244.9.2")));
    }

    #[test]
    fn a_converged_host_needs_no_commands_and_an_empty_one_needs_them_all() {
        let plan = node_routing_plan(&input());
        let mut host = HostRouting {
            rules: plan.rules.clone(),
            routes: BTreeMap::new(),
        };
        for t in &plan.tables {
            host.routes.insert(
                *t,
                plan.routes
                    .iter()
                    .filter(|r| r.table == *t)
                    .cloned()
                    .collect(),
            );
        }
        assert!(diff_routing(&plan, &host).is_empty());

        let cmds = diff_routing(&plan, &HostRouting::default());
        assert_eq!(cmds.len(), plan.routes.len() + plan.rules.len());
        // Routes first, so a rule never points at an empty table.
        assert_eq!(cmds[0][..2], ["route", "replace"]);
        assert!(cmds
            .iter()
            .any(|c| c.join(" ") == "rule add priority 5210 lookup main suppress_prefixlength 10"));
    }

    #[test]
    fn stale_owned_rules_and_routes_are_removed_foreign_ones_are_not() {
        let plan = node_routing_plan(&input());
        let (_, dst) = container_tables("tn");
        let stale_rule = IpRule {
            priority: LOOKUP_PRIORITY,
            selector: RuleSelector::From(net("10.244.7.0/24")),
            action: RuleAction::Lookup(dst),
        };
        let foreign = IpRule {
            priority: 5100,
            selector: RuleSelector::Iif("clex-vpn".into()),
            action: RuleAction::Lookup(5001),
        };
        let stale_route = TableRoute {
            table: dst,
            dest: net("1.0.0.0/8"),
            via: ip("10.244.9.9"),
            dev: "clch-tn".into(),
            src: None,
            onlink: true,
        };
        let mut host = HostRouting {
            rules: plan.rules.clone(),
            routes: BTreeMap::new(),
        };
        host.rules.push(stale_rule);
        host.rules.push(foreign);
        for t in &plan.tables {
            let mut routes: Vec<TableRoute> = plan
                .routes
                .iter()
                .filter(|r| r.table == *t)
                .cloned()
                .collect();
            if *t == dst {
                routes.push(stale_route.clone());
            }
            host.routes.insert(*t, routes);
        }
        let cmds: Vec<String> = diff_routing(&plan, &host)
            .iter()
            .map(|c| c.join(" "))
            .collect();
        assert_eq!(
            cmds,
            vec![
                "rule del priority 5211 from 10.244.7.0/24 lookup ".to_string() + &dst.to_string(),
                format!("route del 1.0.0.0/8 table {dst}"),
            ]
        );
    }

    #[test]
    fn route_commands_name_the_table() {
        let plan = node_routing_plan(&input());
        let (_, dst) = container_tables("tn");
        let cmd = plan
            .routes
            .iter()
            .find(|r| r.table == dst)
            .unwrap()
            .replace_args()
            .join(" ");
        assert_eq!(
            cmd,
            format!(
                "route replace 100.64.0.0/10 via 10.244.9.5 dev clch-tn src 10.244.9.2 onlink table {dst}"
            )
        );
    }

    fn filter_input<'a>(
        open: &'a [(u16, Protocol, u16)],
        proxy: &'a [u16],
        allowed: &'a [Ipv4Net],
    ) -> FilterInput<'a> {
        FilterInput {
            network: "tn",
            bridge: "clbr-x",
            veth: "clch-tn",
            router_ip: Some(ip("10.244.9.5")),
            node_ctn: ip("10.244.9.2"),
            open_ports: open,
            target_addr: Some(ip("192.168.1.2")),
            proxy_ports: proxy,
            allowed,
        }
    }

    #[test]
    fn the_filter_chains_are_scoped_to_the_router_and_end_in_drop() {
        let allowed = [net("192.168.1.0/24")];
        let plan = filter_iptables_plan(&filter_input(
            &[(2222, Protocol::Tcp, 22)],
            &[80, 443],
            &allowed,
        ));
        for name in [input_chain("tn"), forward_chain("tn")] {
            let r = rules(&plan, &name);
            for rule in &r {
                assert!(
                    rule.starts_with("-i clch-tn") || rule.starts_with("-i clbr-x"),
                    "{rule}"
                );
            }
            assert!(r.contains(
                &"-i clbr-x -s 10.244.9.5/32 -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT"
                    .to_string()
            ));
            assert!(r.contains(
                &"-i clch-tn -p tcp -m conntrack --ctorigdstport 2222 -j ACCEPT".to_string()
            ));
            assert!(r.contains(
                &"-i clch-tn -p tcp -m conntrack --ctorigdstport 443 -j ACCEPT".to_string()
            ));
            assert_eq!(r[r.len() - 2], "-i clbr-x -s 10.244.9.5/32 -j DROP");
            assert_eq!(r[r.len() - 1], "-i clch-tn -j DROP");
        }
        let fwd = rules(&plan, &forward_chain("tn"));
        assert_eq!(fwd[0], "-i clbr-x -o clbr-x -j RETURN");
        assert!(fwd.contains(&"-i clch-tn -d 192.168.1.0/24 -j ACCEPT".to_string()));
        // Forwarding to destinations is not an INPUT business.
        assert!(!rules(&plan, &input_chain("tn"))
            .iter()
            .any(|r| r.contains("-d 192.168")));
    }

    #[test]
    fn dnat_for_renumbered_ports_and_the_proxy() {
        let plan = filter_iptables_plan(&filter_input(
            &[(2222, Protocol::Tcp, 22), (53, Protocol::Udp, 53)],
            &[443],
            &[],
        ));
        let nat = rules(&plan, &nat_chain("tn"));
        assert!(nat.contains(
            &"-i clch-tn -p tcp --dport 2222 -m addrtype --dst-type LOCAL -j DNAT --to-destination 192.168.1.2:22"
                .to_string()
        ));
        assert!(nat.contains(
            &"-i clch-tn -p tcp --dport 443 ! -d 10.244.9.2/32 -m addrtype --dst-type LOCAL -j DNAT --to-destination 10.244.9.2"
                .to_string()
        ));
        // Same port number: no DNAT.
        assert!(!nat.iter().any(|r| r.contains("--dport 53")));
        // Without a node address nothing is renumbered.
        let mut i = filter_input(&[(2222, Protocol::Tcp, 22)], &[], &[]);
        i.target_addr = None;
        let plan = filter_iptables_plan(&i);
        assert!(rules(&plan, &nat_chain("tn")).is_empty());
    }

    #[test]
    fn a_proxy_off_network_opens_no_proxy_port() {
        let plan = filter_iptables_plan(&filter_input(&[], &[], &[]));
        assert!(!rules(&plan, &input_chain("tn"))
            .iter()
            .any(|r| r.contains("ctorigdstport")));
        assert!(rules(&plan, &nat_chain("tn")).is_empty());
    }

    #[test]
    fn masquerade_covers_the_routed_subnet_and_router_traffic_to_allowed_nets() {
        let allowed = [net("192.168.1.0/24")];
        let plan = filter_iptables_plan(&filter_input(&[], &[], &allowed));
        assert_eq!(
            rules(&plan, &masq_chain("tn")),
            vec![
                "-o clch-tn ! -s 10.244.9.2/32 -j MASQUERADE",
                "-s 10.244.9.5/32 -d 192.168.1.0/24 -j MASQUERADE"
            ]
        );
    }

    #[test]
    fn the_jumps_run_ahead_of_dockers_rules_and_fall_back_to_forward() {
        let plan = filter_iptables_plan(&filter_input(&[], &[], &[]));
        let by = |c: String| plan.jumps.iter().find(|j| j.chain == c).unwrap();
        assert_eq!(by(input_chain("tn")).parents, vec!["INPUT"]);
        assert!(by(input_chain("tn")).first);
        assert_eq!(
            by(forward_chain("tn")).parents,
            vec!["DOCKER-USER", "FORWARD"]
        );
        assert!(by(forward_chain("tn")).first);
        assert_eq!(by(nat_chain("tn")).parents, vec!["PREROUTING"]);
        assert_eq!(by(masq_chain("tn")).parents, vec!["POSTROUTING"]);
    }

    #[test]
    fn connmark_is_set_on_new_router_connections_and_restored_everywhere() {
        let plan = mangle_iptables_plan("tn", "clbr-x", "clch-tn", Some(ip("10.244.9.5")));
        let v = format!("{:#x}/{MARK_MASK:#x}", container_mark("tn"));
        let pre = rules(&plan, &mangle_chain("tn"));
        assert!(pre[0].contains("--restore-mark"));
        assert!(pre.contains(&format!(
            "-i clbr-x -s 10.244.9.5/32 -m conntrack --ctstate NEW -j CONNMARK --set-xmark {v}"
        )));
        assert!(pre.contains(&format!(
            "-i clch-tn -m conntrack --ctstate NEW -j CONNMARK --set-xmark {v}"
        )));
        let out = rules(&plan, &mangle_output_chain("tn"));
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("--restore-mark"));
        let parents: Vec<&str> = plan.jumps.iter().map(|j| j.parents[0].as_str()).collect();
        assert_eq!(parents, vec!["PREROUTING", "OUTPUT"]);
        assert!(plan.jumps.iter().all(|j| j.table == "mangle"));
    }

    #[test]
    fn l2_isolation_accepts_only_router_and_node_addresses() {
        let plan = l2_isolation_plan(
            "clbr-x",
            Some(ip("10.244.9.5")),
            &[ip("10.244.9.2"), ip("10.244.9.3")],
        );
        let r = rules(&plan, &l2_chain("clbr-x"));
        assert_eq!(r.len(), 3 * 2 + 1);
        assert!(r.contains(&"-i clbr-x -o clbr-x -s 10.244.9.5/32 -j ACCEPT".to_string()));
        assert!(r.contains(&"-i clbr-x -o clbr-x -d 10.244.9.3/32 -j ACCEPT".to_string()));
        assert_eq!(r.last().unwrap(), "-i clbr-x -o clbr-x -j DROP");
        assert_eq!(plan.jumps[0].parents, vec!["DOCKER-USER", "FORWARD"]);
        // No router yet: clients can only talk to node addresses.
        let r = rules(&l2_isolation_plan("clbr-x", None, &[]), &l2_chain("clbr-x"));
        assert_eq!(r, vec!["-i clbr-x -o clbr-x -j DROP"]);
    }

    #[test]
    fn client_routes_fail_closed_without_a_router() {
        let v = |c: Vec<Vec<String>>| c.iter().map(|a| a.join(" ")).collect::<Vec<_>>();
        assert_eq!(
            v(client_netns_routes(
                net("100.64.0.0/10"),
                Some(ip("10.244.9.5"))
            )),
            vec!["route replace 100.64.0.0/10 via 10.244.9.5"]
        );
        assert_eq!(
            v(client_netns_routes(
                net("0.0.0.0/0"),
                Some(ip("10.244.9.5"))
            )),
            vec!["route replace default via 10.244.9.5"]
        );
        assert_eq!(
            v(client_netns_routes(net("100.64.0.0/10"), None)),
            vec!["route replace prohibit 100.64.0.0/10"]
        );
        assert_eq!(
            v(client_netns_routes(net("0.0.0.0/0"), None)),
            vec!["route replace prohibit default"]
        );
    }

    #[test]
    fn router_routes_reach_each_node_directly_and_lan_hosts_via_the_local_node() {
        let cmds: Vec<String> = router_netns_routes(
            &[
                (ip("192.168.1.3"), ip("10.244.9.3")),
                (ip("192.168.1.2"), ip("10.244.9.2")),
                (ip("192.168.1.2"), ip("10.244.9.2")),
            ],
            &[net("192.168.1.0/24")],
            ip("10.244.9.2"),
        )
        .iter()
        .map(|a| a.join(" "))
        .collect();
        assert_eq!(
            cmds,
            vec![
                "route replace 192.168.1.2/32 via 10.244.9.2",
                "route replace 192.168.1.3/32 via 10.244.9.3",
                "route replace 192.168.1.0/24 via 10.244.9.2",
            ]
        );
    }

    #[test]
    fn the_input_jump_is_ordered_before_the_container_pool_drop() {
        let plan = filter_iptables_plan(&FilterInput {
            network: "tn",
            bridge: "clbr-x",
            veth: "clch-tn",
            router_ip: None,
            node_ctn: ip("10.244.9.2"),
            open_ports: &[],
            target_addr: None,
            proxy_ports: &[],
            allowed: &[],
        });
        let input = plan
            .jumps
            .iter()
            .find(|j| j.parents == ["INPUT"])
            .expect("an INPUT jump");
        assert_eq!(
            input.before.as_deref(),
            Some(crate::egress::CONTAINER_INPUT_CHAIN)
        );
    }
}
