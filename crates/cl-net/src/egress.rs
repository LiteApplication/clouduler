//! Egress networks: a node-local bridge whose only route out is a named host
//! interface (a VPN tunnel, usually), with an iptables kill-switch so nothing
//! from it ever leaves any other way.
//!
//! Unlike the VXLAN overlay this needs no cluster-wide coordination: the
//! bridge never carries traffic between nodes, so each node implementing an
//! egress network just picks its own subnet, deterministically from the
//! network's name, and never has to agree with anyone else about it.
//!
//! The pure logic (naming, subnet derivation, the iptables command list)
//! lives at the top of this file and has no process/netlink dependency, so
//! it is unit tested without privileges. The privileged glue at the bottom
//! needs CAP_NET_ADMIN and a real `iptables` binary.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};
use std::process::Command;

use cl_config::{Protocol, EGRESS_POOL_BASE, NAT_POOL_BASE};
use ipnet::Ipv4Net;

use crate::error::{NetError, Result};
use crate::iptables::{self, ChainSpec, IptablesPlan, Jump};
use crate::vxlan::Overlay;

/// Linux interface names are capped at `IFNAMSIZ - 1` usable bytes.
///
/// Shared with `wireguard`, whose interface names are capped the same way.
pub(crate) const IFNAMSIZ_USABLE: usize = 15;

/// iptables chain names are capped at 28 bytes.
///
/// Shared with `wireguard`, whose ingress chains are capped the same way.
pub(crate) const CHAIN_USABLE: usize = 28;

/// The `ip rule` priority of every egress bridge's policy rule.
pub const POLICY_RULE_PRIORITY: u32 = 5100;

/// Chain-name prefixes this module and `wireguard` create. The orphan sweep
/// touches chains with these prefixes and nothing else.
const NAT_CHAIN_PREFIX: &str = "CLDR-NAT-";
const FWD_CHAIN_PREFIX: &str = "CLDR-FWD-";

/// The docker network name for an egress-kind cluster network. Global, not
/// stack-scoped: unlike an overlay network, an egress network is shared
/// cluster infrastructure that any stack can attach a service to.
pub fn egress_docker_name(network: &str) -> String {
    format!("egress_{network}")
}

/// The docker network name for a nat-kind cluster network. Same idea as
/// `egress_docker_name`, its own prefix only so the two read apart in
/// `docker network ls`.
pub fn nat_docker_name(network: &str) -> String {
    format!("nat_{network}")
}

/// Deterministic interface name for an egress network's bridge.
pub fn egress_bridge_name(network: &str) -> String {
    short_name("clex-", network, IFNAMSIZ_USABLE)
}

/// The nat-table chain holding an egress bridge's masquerade rule.
pub fn egress_nat_chain(bridge: &str) -> String {
    short_name(NAT_CHAIN_PREFIX, bridge, CHAIN_USABLE)
}

/// The filter chain holding an egress bridge's kill-switch.
pub fn egress_fwd_chain(bridge: &str) -> String {
    short_name(FWD_CHAIN_PREFIX, bridge, CHAIN_USABLE)
}

/// Build a `<= cap` byte name from a prefix and a network name. Short names
/// pass through unchanged; long ones get truncated and a hash suffix
/// appended so two networks that truncate to the same prefix don't collide.
///
/// Shared with `wireguard` and `vxlan`, which derive their names the same way.
pub(crate) fn short_name(prefix: &str, name: &str, cap: usize) -> String {
    let full = format!("{prefix}{name}");
    if full.len() <= cap {
        return full;
    }
    let suffix = format!("{:04x}", (fnv1a(name.as_bytes()) & 0xffff) as u16);
    let budget = cap.saturating_sub(prefix.len() + suffix.len());
    // The cap is in bytes, so a multibyte name has to be cut on a character
    // boundary by byte count, not by taking `budget` characters.
    let mut truncated = String::new();
    for c in name.chars() {
        if truncated.len() + c.len_utf8() > budget {
            break;
        }
        truncated.push(c);
    }
    format!("{prefix}{truncated}{suffix}")
}

fn fnv1a(bytes: &[u8]) -> u64 {
    cl_config::fnv1a(bytes)
}

/// A node-local `/24` for this network's bridge, deterministically derived
/// from its name inside the reserved `10.245.0.0/16` pool. No coordination
/// needed: this subnet is never routed to another node. Two names can derive
/// the same subnet; `ClusterConfig::validate` refuses such a config.
pub fn egress_subnet(network: &str) -> Ipv4Net {
    let second_octet = cl_config::subnet_octet(network);
    format!("10.{EGRESS_POOL_BASE}.{second_octet}.0/24")
        .parse()
        .expect("a /24 built from two bytes always parses")
}

/// A node-local `/24` for a nat network's docker bridge, deterministically
/// derived from its name inside its own reserved pool, same idea as
/// `egress_subnet`. Explicit rather than left to docker's own IPAM so that a
/// same-node client hairpinning back through it - see `gate_by_network` in
/// `cl-proxy` and `trusted_overlay_subnets` in the daemon's proxy wiring -
/// arrives from an address every node can compute and trust without any
/// cluster-wide coordination.
pub fn nat_subnet(network: &str) -> Ipv4Net {
    let second_octet = cl_config::subnet_octet(network);
    format!("10.{NAT_POOL_BASE}.{second_octet}.0/24")
        .parse()
        .expect("a /24 built from two bytes always parses")
}

/// Every address pool container traffic can originate from on this host: the
/// overlay pool, the egress pool and the nat pool. Anything sourced from one
/// of these reaching the host's own `INPUT` chain is a container talking to
/// the host, never a trusted node.
pub fn container_pools() -> Vec<Ipv4Net> {
    vec![
        overlay_pool(),
        format!("10.{EGRESS_POOL_BASE}.0.0/16")
            .parse()
            .expect("a /16 built from two bytes always parses"),
        format!("10.{NAT_POOL_BASE}.0.0/16")
            .parse()
            .expect("a /16 built from two bytes always parses"),
    ]
}

/// The pool stack overlay networks are carved from.
pub fn overlay_pool() -> Ipv4Net {
    crate::ipam::DEFAULT_OVERLAY_POOL
        .parse()
        .expect("the default overlay pool parses")
}

/// The bridge's own address on its subnet: the first host address.
pub fn egress_gateway(subnet: Ipv4Net) -> Ipv4Addr {
    let base = subnet.network();
    let octets = base.octets();
    Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3] + 1)
}

/// The rules that enforce the kill-switch: NAT egress-network traffic out
/// `iface` and nothing else, drop anything from the bridge that doesn't leave
/// that way.
///
/// `iface` is `None` while the interface has no usable route (down, no
/// gateway). The bridge is then locked down completely rather than left to
/// whatever the main routing table would do with it: no masquerade, no
/// accept, just the drop.
///
/// Each network gets its own chain pair, replaced in one atomic
/// `iptables-restore` rather than flushed and appended to, so there is never
/// an instant where the kill-switch is empty.
///
/// The filter chain is jumped to from the top of `DOCKER-USER` (or of
/// `FORWARD` when docker has no such chain). It has to come first: docker's
/// own rules accept forwarded traffic from its bridges, and a kill-switch
/// evaluated after them would never see it.
pub fn egress_iptables_plan(bridge: &str, subnet: Ipv4Net, iface: Option<&str>) -> IptablesPlan {
    let nat_name = egress_nat_chain(bridge);
    let fwd_name = egress_fwd_chain(bridge);
    let subnet = subnet.to_string();

    let mut nat = ChainSpec::new("nat", &nat_name);
    let mut fwd = ChainSpec::new("filter", &fwd_name);

    // Replies to connections someone else opened towards a container on this
    // bridge (a docker-published port, say). Running ahead of docker's rules
    // means this chain now has to let those replies out itself, or the
    // bridge-wide drop below would eat them. Restricted to the reply
    // direction so a connection the container itself opened never rides this
    // rule out of the wrong interface.
    fwd.push(&[
        "-i",
        bridge,
        "-m",
        "conntrack",
        "--ctstate",
        "ESTABLISHED,RELATED",
        "--ctdir",
        "REPLY",
        "-j",
        "ACCEPT",
    ]);

    if let Some(iface) = iface {
        nat.push(&["-s", &subnet, "-o", iface, "-j", "MASQUERADE"]);
        fwd.push(&["-i", bridge, "-o", iface, "-j", "ACCEPT"]);
        fwd.push(&[
            "-i",
            iface,
            "-o",
            bridge,
            "-m",
            "conntrack",
            "--ctstate",
            "ESTABLISHED,RELATED",
            "-j",
            "ACCEPT",
        ]);
    }
    // Anything from the bridge that didn't just get accepted above is dropped
    // here: the kill-switch. No interface, no route out, ever, from this
    // chain's point of view.
    fwd.push(&["-i", bridge, "-j", "DROP"]);

    IptablesPlan {
        chains: vec![nat, fwd],
        jumps: vec![
            Jump::new("nat", &["POSTROUTING"], &nat_name, false),
            Jump::new("filter", &["DOCKER-USER", "FORWARD"], &fwd_name, true),
        ],
    }
}

/// The nat chain `overlay_snat_plan` fills.
pub const OVERLAY_SNAT_CHAIN: &str = "CLDR-OVL-SNAT";

/// Source-NATs a container's overlay address when it talks to another node's
/// proxy on the internal network.
///
/// Overlay networks are created with docker's own masquerade off, so a packet
/// from an overlay container to a node address (where that node's reverse
/// proxy answers for every route published on the network) leaves this host
/// still carrying the overlay source. The underlay (a tailnet, say) drops it,
/// so the proxy was only reachable on the container's own node. Masquerading
/// `overlay_pool` -> `node_addrs` makes the remote proxy see this node's own
/// internal address, which sits inside the network's subnet and passes the
/// proxy's network gate.
///
/// Only TCP to `proxy_ports` is NATed. That is all this exists for, and
/// masquerading every port would launder a container's traffic into a trusted
/// node address on any service the other node listens on (its API, raft, the
/// drive endpoints), which those services take as proof of a real node. The
/// other node's own `CLDR-IN` filter lets the proxy ports through to
/// containers and nothing else.
///
/// ICMP echo requests are NATed too, so a container can ping every node by
/// its internal address, not only its own. An echo reply is all that comes
/// back, and every node's `CLDR-IN` already accepts echo requests.
///
/// A node's own address is never listed: it is delivered locally, before
/// `POSTROUTING`.
pub fn overlay_snat_plan(
    pool: Ipv4Net,
    node_addrs: &[Ipv4Addr],
    proxy_ports: &[u16],
) -> IptablesPlan {
    let mut chain = ChainSpec::new("nat", OVERLAY_SNAT_CHAIN);
    let pool = pool.to_string();
    let ports: BTreeSet<u16> = proxy_ports.iter().copied().collect();
    let ports = ports
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    for addr in node_addrs {
        let addr = format!("{addr}/32");
        if !ports.is_empty() {
            chain.push(&[
                "-s",
                &pool,
                "-d",
                &addr,
                "-p",
                "tcp",
                "-m",
                "multiport",
                "--dports",
                &ports,
                "-j",
                "MASQUERADE",
            ]);
        }
        chain.push(&[
            "-s",
            &pool,
            "-d",
            &addr,
            "-p",
            "icmp",
            "--icmp-type",
            "echo-request",
            "-j",
            "MASQUERADE",
        ]);
    }
    IptablesPlan {
        chains: vec![chain],
        jumps: vec![Jump::new(
            "nat",
            &["POSTROUTING"],
            OVERLAY_SNAT_CHAIN,
            false,
        )],
    }
}

/// The filter chain `container_input_plan` fills.
pub const CONTAINER_INPUT_CHAIN: &str = "CLDR-IN";

/// Host services a container is allowed to reach, besides DNS.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostServices {
    /// TCP ports of the reverse proxy (http and https).
    pub proxy_ports: Vec<u16>,
    /// Non-HTTP ports clouduler itself publishes on every node
    /// (`x-clouduler.expose`).
    pub published: Vec<(Protocol, u16)>,
}

/// The `INPUT` filter that keeps containers away from the host's own
/// listeners.
///
/// Bridges carry host addresses (`10.244.x.1`, `10.245.x.1`, `10.246.x.1`), so
/// anything the host listens on at `0.0.0.0` (the API, the raft RPC, the drive
/// endpoints, a docker socket on tcp) is reachable from every container unless
/// the host refuses. This chain is jumped to from position 1 of `INPUT`, and
/// matches by source subnet (`pools`), not by interface, so a container cannot
/// dodge it by arriving on a bridge nobody listed.
///
/// What a container may reach, and why; everything else from `pools` is
/// dropped:
///
/// * Replies to connections the host opened (`ESTABLISHED,RELATED`): the
///   proxy talking to a container, health checks, `docker exec`.
/// * DNS (udp and tcp 53) to an overlay gateway address: the internal
///   resolver is bound on each overlay bridge's gateway and handed to every
///   container.
/// * The reverse proxy's http and https ports: containers reach routes
///   published on the internal network through a node address, and the
///   same-node nat hairpin lands here too.
/// * Ports clouduler publishes with `x-clouduler.expose`, whose forwarders
///   listen on every node's addresses.
/// * ICMP echo, so a container can tell whether its gateway is up.
///
/// Docker `-p` published ports are not listed: traffic to them is
/// DNAT-ed in `PREROUTING` and forwarded, never delivered to `INPUT`.
pub fn container_input_plan(
    overlay_pool: Ipv4Net,
    pools: &[Ipv4Net],
    services: &HostServices,
) -> IptablesPlan {
    let mut chain = ChainSpec::new("filter", CONTAINER_INPUT_CHAIN);
    let overlay = overlay_pool.to_string();

    chain.push(&[
        "-m",
        "conntrack",
        "--ctstate",
        "ESTABLISHED,RELATED",
        "-j",
        "ACCEPT",
    ]);

    let proxy_ports: BTreeSet<u16> = services.proxy_ports.iter().copied().collect();
    let published: BTreeSet<(String, u16)> = services
        .published
        .iter()
        .map(|(proto, port)| (proto.to_string(), *port))
        .collect();

    for pool in pools {
        let src = pool.to_string();
        for proto in ["udp", "tcp"] {
            chain.push(&[
                "-s", &src, "-d", &overlay, "-p", proto, "--dport", "53", "-j", "ACCEPT",
            ]);
        }
        if !proxy_ports.is_empty() {
            let ports = proxy_ports
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(",");
            chain.push(&[
                "-s",
                &src,
                "-p",
                "tcp",
                "-m",
                "multiport",
                "--dports",
                &ports,
                "-j",
                "ACCEPT",
            ]);
        }
        for (proto, port) in &published {
            chain.push(&[
                "-s",
                &src,
                "-p",
                proto,
                "--dport",
                &port.to_string(),
                "-j",
                "ACCEPT",
            ]);
        }
        chain.push(&[
            "-s",
            &src,
            "-p",
            "icmp",
            "--icmp-type",
            "echo-request",
            "-j",
            "ACCEPT",
        ]);
    }
    for pool in pools {
        chain.push(&["-s", &pool.to_string(), "-j", "DROP"]);
    }

    IptablesPlan {
        chains: vec![chain],
        jumps: vec![Jump::new("filter", &["INPUT"], CONTAINER_INPUT_CHAIN, true)],
    }
}

/// Ensure the bridge exists, with the gateway address for `subnet`, up. No
/// VXLAN device: unlike a stack overlay this bridge is node-local only.
pub async fn ensure_egress_bridge(
    overlay: &Overlay,
    network: &str,
    subnet: Ipv4Net,
) -> Result<String> {
    let bridge = egress_bridge_name(network);
    let gateway = egress_gateway(subnet);
    overlay
        .ensure_bridge(&bridge, gateway, subnet.prefix_len(), 1500)
        .await?;
    Ok(bridge)
}

/// How long to wait for a reply before treating the primary interface as
/// down. Short: this runs on every reconcile tick, and a node with a truly
/// dead interface should not stall the rest of that tick's work on it.
const PROBE_TIMEOUT_SECS: u32 = 2;

/// Args for a single ICMP echo sent out `iface` to `target`, used to decide
/// whether an egress network's primary interface still reaches the
/// internet. Pure and unit tested; `probe_reachable` below is the privileged
/// half that actually runs it.
pub fn ping_plan(iface: &str, target: IpAddr) -> Vec<String> {
    argv(&[
        "-I",
        iface,
        "-c",
        "1",
        "-W",
        &PROBE_TIMEOUT_SECS.to_string(),
        &target.to_string(),
    ])
}

/// True if `target` answered a single ICMP echo sent out `iface`. Needs the
/// `ping` binary (iputils) and CAP_NET_RAW, same as the rest of this module
/// needs CAP_NET_ADMIN — `ping` handles the raw socket itself. A missing
/// binary, a permission error, and a real timeout are all indistinguishable
/// here on purpose: any of them means "cannot confirm this interface still
/// works," which is exactly when a caller wants to fail over.
pub fn probe_reachable(iface: &str, target: IpAddr) -> bool {
    Command::new("ping")
        .args(ping_plan(iface, target))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// `ifalias` clouduler stamps on every wireguard link it creates, so a sweep
/// can tell its tunnels from one the operator made by hand: an egress network
/// may legitimately point at an admin-managed interface that happens to be
/// called `wg-something`.
pub const WIREGUARD_LINK_ALIAS: &str = "clouduler:wireguard";

/// Every egress-side object this node should currently have.
#[derive(Debug, Clone, Default)]
pub struct Wanted {
    /// Bridge names (`egress_bridge_name`) of every network this node
    /// implements, plus any whose docker network could not be removed yet and
    /// whose kill-switch must therefore stay up.
    pub bridges: BTreeSet<String>,
    /// Wireguard interface names this node should hold.
    pub wg_ifaces: BTreeSet<String>,
}

/// One `ip rule` of ours, as `ip rule show` prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyRule {
    pub priority: u32,
    pub iif: String,
    pub table: u32,
}

/// What currently exists on the host that a sweep might remove.
#[derive(Debug, Clone, Default)]
pub struct HostObjects {
    pub filter_chains: Vec<String>,
    pub nat_chains: Vec<String>,
    pub policy_rules: Vec<PolicyRule>,
    /// `clex-*` links that are bridges.
    pub bridges: Vec<String>,
    /// Links carrying [`WIREGUARD_LINK_ALIAS`].
    pub wg_links: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepAction {
    /// Unhook every jump, flush, delete.
    RemoveChain {
        table: &'static str,
        chain: String,
    },
    DelRule(PolicyRule),
    FlushTable(u32),
    DelLink(String),
}

/// Chains this crate creates and sweeps, by name prefix.
fn is_swept_chain(name: &str) -> bool {
    [
        NAT_CHAIN_PREFIX,
        FWD_CHAIN_PREFIX,
        crate::wireguard::INGRESS_CHAIN_PREFIX,
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

/// Decide what to remove. Pure, so what is and is not touched is tested
/// without root.
///
/// Conservative on purpose: only objects with clouduler's own naming prefixes
/// (and, for wireguard links, its alias) are ever considered, and only those
/// whose network is not in `wanted`. Removal is ordered chains, ip rules,
/// routing tables, then links, so nothing is deleted while something still
/// refers to it.
pub fn plan_sweep(host: &HostObjects, wanted: &Wanted) -> Vec<SweepAction> {
    let mut wanted_chains: BTreeSet<String> = BTreeSet::new();
    for bridge in &wanted.bridges {
        wanted_chains.insert(egress_nat_chain(bridge));
        wanted_chains.insert(egress_fwd_chain(bridge));
    }
    for iface in &wanted.wg_ifaces {
        wanted_chains.insert(crate::wireguard::wireguard_ingress_chain(iface));
    }

    let mut actions = Vec::new();
    for (table, chains) in [("filter", &host.filter_chains), ("nat", &host.nat_chains)] {
        for chain in chains {
            if is_swept_chain(chain) && !wanted_chains.contains(chain) {
                actions.push(SweepAction::RemoveChain {
                    table,
                    chain: chain.clone(),
                });
            }
        }
    }

    let mut flushed = BTreeSet::new();
    for rule in &host.policy_rules {
        if rule.priority == POLICY_RULE_PRIORITY
            && rule.iif.starts_with("clex-")
            && !wanted.bridges.contains(&rule.iif)
        {
            actions.push(SweepAction::DelRule(rule.clone()));
            if flushed.insert(rule.table) {
                actions.push(SweepAction::FlushTable(rule.table));
            }
        }
    }

    for bridge in &host.bridges {
        if bridge.starts_with("clex-") && !wanted.bridges.contains(bridge) {
            actions.push(SweepAction::DelLink(bridge.clone()));
        }
    }
    for link in &host.wg_links {
        if !wanted.wg_ifaces.contains(link) {
            actions.push(SweepAction::DelLink(link.clone()));
        }
    }
    actions
}

/// Parse `ip rule show`: `5100:\tfrom all iif clex-vpn lookup 5123`.
pub fn parse_policy_rules(output: &str) -> Vec<PolicyRule> {
    output
        .lines()
        .filter_map(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            let priority = tokens.first()?.strip_suffix(':')?.parse().ok()?;
            let after = |key: &str| {
                tokens
                    .iter()
                    .position(|t| *t == key)
                    .and_then(|i| tokens.get(i + 1))
            };
            Some(PolicyRule {
                priority,
                iif: (*after("iif")?).to_string(),
                table: after("lookup")?.parse().ok()?,
            })
        })
        .collect()
}

/// Read what exists on the host. Needs root, like everything here.
pub fn scan_host() -> Result<HostObjects> {
    let rules = Command::new("ip")
        .args(["rule", "show"])
        .output()
        .map_err(|e| NetError::Io {
            context: "ip rule show".to_string(),
            source: e,
        })?;
    let mut host = HostObjects {
        filter_chains: iptables::list_chains("filter")?,
        nat_chains: iptables::list_chains("nat")?,
        policy_rules: parse_policy_rules(&String::from_utf8_lossy(&rules.stdout)),
        ..HostObjects::default()
    };
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if name.starts_with("clex-") && path.join("bridge").exists() {
                host.bridges.push(name.clone());
            }
            let alias = std::fs::read_to_string(path.join("ifalias")).unwrap_or_default();
            if alias.trim() == WIREGUARD_LINK_ALIAS {
                host.wg_links.push(name);
            }
        }
    }
    Ok(host)
}

/// Outcome of [`run_sweep`]: what was removed, and what failed.
#[derive(Debug, Default)]
pub struct SweepReport {
    pub removed: Vec<String>,
    pub failed: Vec<String>,
}

/// Carry out [`plan_sweep`]'s actions. One failure does not stop the rest, and
/// a failed step is simply tried again on the next tick.
pub fn run_sweep(actions: &[SweepAction]) -> SweepReport {
    let mut report = SweepReport::default();
    for action in actions {
        let (what, result) = match action {
            SweepAction::RemoveChain { table, chain } => (
                format!("{table} chain {chain}"),
                iptables::remove_chain(table, chain),
            ),
            SweepAction::DelRule(rule) => (
                format!("ip rule for {}", rule.iif),
                run_ip(&[
                    "rule",
                    "del",
                    "iif",
                    &rule.iif,
                    "lookup",
                    &rule.table.to_string(),
                    "priority",
                    &rule.priority.to_string(),
                ]),
            ),
            SweepAction::FlushTable(table) => (
                format!("routing table {table}"),
                run_ip(&["route", "flush", "table", &table.to_string()]),
            ),
            SweepAction::DelLink(name) => (format!("link {name}"), run_ip(&["link", "del", name])),
        };
        match result {
            Ok(()) => report.removed.push(what),
            Err(e) => report.failed.push(format!("{what}: {e}")),
        }
    }
    report
}

fn run_ip(args: &[&str]) -> Result<()> {
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
    // Already gone is the goal, not a failure.
    if stderr.contains("Cannot find device") || stderr.contains("No such file") {
        return Ok(());
    }
    Err(NetError::Io {
        context: format!("ip {}: {stderr}", args.join(" ")),
        source: std::io::Error::other("ip exited non-zero"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(plan: &IptablesPlan, chain: &str) -> Vec<String> {
        plan.chain(chain)
            .unwrap_or_else(|| panic!("no chain {chain}"))
            .rules
            .clone()
    }

    #[test]
    fn a_short_network_name_passes_through() {
        assert_eq!(egress_bridge_name("vpn"), "clex-vpn");
        assert_eq!(egress_docker_name("vpn"), "egress_vpn");
        assert_eq!(nat_docker_name("vpn"), "nat_vpn");
    }

    #[test]
    fn a_long_network_name_is_truncated_with_a_hash_suffix() {
        let name = egress_bridge_name("a-very-long-egress-network-name");
        assert!(name.len() <= IFNAMSIZ_USABLE, "{name} exceeds IFNAMSIZ");
        assert!(name.starts_with("clex-"));
    }

    #[test]
    fn truncation_counts_bytes_not_characters() {
        let name = egress_bridge_name("ééééééééééééé");
        assert!(
            name.len() <= IFNAMSIZ_USABLE,
            "{name} is {} bytes",
            name.len()
        );
    }

    #[test]
    fn the_subnet_is_deterministic_and_inside_the_reserved_pool() {
        let a = egress_subnet("vpn");
        let b = egress_subnet("vpn");
        assert_eq!(a, b);
        assert!(a.addr().octets()[0] == 10 && a.addr().octets()[1] == EGRESS_POOL_BASE);
        assert_eq!(a.prefix_len(), 24);
    }

    #[test]
    fn two_different_networks_usually_get_different_subnets() {
        assert_ne!(egress_subnet("vpn"), egress_subnet("vpn2"));
    }

    #[test]
    fn a_nat_subnet_is_deterministic_and_never_collides_with_egress() {
        let a = nat_subnet("home");
        let b = nat_subnet("home");
        assert_eq!(a, b);
        assert!(a.addr().octets()[0] == 10 && a.addr().octets()[1] == NAT_POOL_BASE);
        assert_ne!(NAT_POOL_BASE, EGRESS_POOL_BASE);
    }

    #[test]
    fn the_container_pools_cover_every_subnet_a_bridge_can_be_given() {
        let pools = container_pools();
        assert!(pools.iter().any(|p| p.contains(&egress_subnet("vpn"))));
        assert!(pools.iter().any(|p| p.contains(&nat_subnet("home"))));
        assert!(pools.iter().any(|p| p.contains(&overlay_pool())));
    }

    #[test]
    fn the_gateway_is_the_first_host_address() {
        let subnet: Ipv4Net = "10.245.7.0/24".parse().unwrap();
        assert_eq!(egress_gateway(subnet), Ipv4Addr::new(10, 245, 7, 1));
    }

    #[test]
    fn the_ruleset_only_accepts_traffic_out_the_named_interface() {
        let plan = egress_iptables_plan("clex-vpn", "10.245.7.0/24".parse().unwrap(), Some("wg0"));
        let fwd = rules(&plan, &egress_fwd_chain("clex-vpn"));
        assert!(fwd
            .iter()
            .any(|r| r.contains("-o wg0") && r.ends_with("-j ACCEPT")));
        // The drop has to come after the accept-out-wg0 rule, or it would
        // never be reached; iptables evaluates in order.
        let drop_pos = fwd.iter().position(|r| r.ends_with("-j DROP")).unwrap();
        let accept_pos = fwd.iter().position(|r| r.contains("-o wg0")).unwrap();
        assert!(accept_pos < drop_pos);
        assert_eq!(drop_pos, fwd.len() - 1, "the drop must be last");
    }

    #[test]
    fn replies_to_inbound_connections_are_let_out_but_only_in_the_reply_direction() {
        let plan = egress_iptables_plan("clex-vpn", "10.245.7.0/24".parse().unwrap(), Some("wg0"));
        let fwd = rules(&plan, &egress_fwd_chain("clex-vpn"));
        assert!(fwd[0].contains("--ctdir REPLY") && fwd[0].contains("-i clex-vpn"));
    }

    #[test]
    fn without_an_interface_the_bridge_is_locked_down() {
        let plan = egress_iptables_plan("clex-vpn", "10.245.7.0/24".parse().unwrap(), None);
        let fwd = rules(&plan, &egress_fwd_chain("clex-vpn"));
        assert_eq!(fwd.last().unwrap(), "-i clex-vpn -j DROP");
        assert!(!fwd.iter().any(|r| r.contains("-o ")));
        assert!(rules(&plan, &egress_nat_chain("clex-vpn")).is_empty());
    }

    #[test]
    fn the_kill_switch_runs_before_dockers_own_forward_rules() {
        let plan = egress_iptables_plan("clex-vpn", "10.245.7.0/24".parse().unwrap(), Some("wg0"));
        let jump = plan
            .jumps
            .iter()
            .find(|j| j.chain == egress_fwd_chain("clex-vpn"))
            .unwrap();
        assert!(jump.first);
        assert_eq!(jump.parents, vec!["DOCKER-USER", "FORWARD"]);
    }

    #[test]
    fn the_masquerade_rule_scopes_to_the_subnet_and_interface() {
        let plan = egress_iptables_plan("clex-vpn", "10.245.7.0/24".parse().unwrap(), Some("wg0"));
        let nat = rules(&plan, &egress_nat_chain("clex-vpn"));
        assert_eq!(nat, vec!["-s 10.245.7.0/24 -o wg0 -j MASQUERADE"]);
    }

    #[test]
    fn the_overlay_snat_only_masquerades_proxy_ports_and_pings_towards_the_listed_nodes() {
        let plan = overlay_snat_plan(
            "10.244.0.0/16".parse().unwrap(),
            &[Ipv4Addr::new(100, 64, 0, 1), Ipv4Addr::new(100, 64, 0, 2)],
            &[443, 80, 443],
        );
        let rules = rules(&plan, OVERLAY_SNAT_CHAIN);
        assert_eq!(
            rules,
            vec![
                "-s 10.244.0.0/16 -d 100.64.0.1/32 -p tcp -m multiport --dports 80,443 -j MASQUERADE",
                "-s 10.244.0.0/16 -d 100.64.0.1/32 -p icmp --icmp-type echo-request -j MASQUERADE",
                "-s 10.244.0.0/16 -d 100.64.0.2/32 -p tcp -m multiport --dports 80,443 -j MASQUERADE",
                "-s 10.244.0.0/16 -d 100.64.0.2/32 -p icmp --icmp-type echo-request -j MASQUERADE",
            ]
        );
        // With the proxy off a node is still pingable.
        let no_proxy = overlay_snat_plan(
            "10.244.0.0/16".parse().unwrap(),
            &[Ipv4Addr::new(100, 64, 0, 1)],
            &[],
        );
        assert_eq!(
            self::rules(&no_proxy, OVERLAY_SNAT_CHAIN),
            vec![
                "-s 10.244.0.0/16 -d 100.64.0.1/32 -p icmp --icmp-type echo-request -j MASQUERADE"
            ]
        );
        // No address, no rule: the chain is still replaced so a removed node
        // stops being NATed.
        let empty = overlay_snat_plan("10.244.0.0/16".parse().unwrap(), &[], &[80]);
        assert!(self::rules(&empty, OVERLAY_SNAT_CHAIN).is_empty());
    }

    fn input_plan() -> IptablesPlan {
        container_input_plan(
            overlay_pool(),
            &container_pools(),
            &HostServices {
                proxy_ports: vec![80, 443],
                published: vec![(Protocol::Tcp, 2222), (Protocol::Udp, 51413)],
            },
        )
    }

    #[test]
    fn the_container_input_filter_sits_first_in_input() {
        let plan = input_plan();
        assert_eq!(plan.jumps.len(), 1);
        assert_eq!(plan.jumps[0].parents, vec!["INPUT"]);
        assert!(plan.jumps[0].first);
    }

    #[test]
    fn the_container_input_filter_allows_exactly_the_documented_services() {
        let rules = rules(&input_plan(), CONTAINER_INPUT_CHAIN);
        assert_eq!(
            rules[0],
            "-m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT"
        );
        let overlay = "-s 10.244.0.0/16";
        assert!(rules.contains(&format!(
            "{overlay} -d 10.244.0.0/16 -p udp --dport 53 -j ACCEPT"
        )));
        assert!(rules.contains(&format!(
            "{overlay} -d 10.244.0.0/16 -p tcp --dport 53 -j ACCEPT"
        )));
        assert!(rules.contains(&format!(
            "{overlay} -p tcp -m multiport --dports 80,443 -j ACCEPT"
        )));
        assert!(rules.contains(&format!("{overlay} -p tcp --dport 2222 -j ACCEPT")));
        assert!(rules.contains(&format!("{overlay} -p udp --dport 51413 -j ACCEPT")));
        // The nat and egress pools get the same treatment.
        assert!(rules.contains(
            &"-s 10.246.0.0/16 -p tcp -m multiport --dports 80,443 -j ACCEPT".to_string()
        ));
        // Nothing accepts a port that was not listed.
        assert!(!rules.iter().any(|r| r.contains("--dport 7000")));
    }

    #[test]
    fn the_container_input_filter_drops_every_pool_after_all_the_accepts() {
        let rules = rules(&input_plan(), CONTAINER_INPUT_CHAIN);
        let drops: Vec<&String> = rules.iter().filter(|r| r.ends_with("-j DROP")).collect();
        assert_eq!(
            drops,
            vec![
                "-s 10.244.0.0/16 -j DROP",
                "-s 10.245.0.0/16 -j DROP",
                "-s 10.246.0.0/16 -j DROP"
            ]
        );
        let first_drop = rules.iter().position(|r| r.ends_with("-j DROP")).unwrap();
        assert!(rules[first_drop..].iter().all(|r| r.ends_with("-j DROP")));
    }

    #[test]
    fn a_disabled_proxy_opens_no_proxy_port() {
        let plan =
            container_input_plan(overlay_pool(), &container_pools(), &HostServices::default());
        assert!(!rules(&plan, CONTAINER_INPUT_CHAIN)
            .iter()
            .any(|r| r.contains("multiport")));
    }

    #[test]
    fn the_ping_plan_binds_to_the_given_interface() {
        let plan = ping_plan("wg0", "1.1.1.1".parse().unwrap());
        assert!(plan.windows(2).any(|w| w == ["-I", "wg0"]));
        assert!(plan.last().map(|s| s == "1.1.1.1").unwrap_or(false));
    }

    #[test]
    fn a_missing_ping_binary_is_treated_as_unreachable_not_a_panic() {
        // Regardless of environment, this must never panic; on a host with no
        // `ping` on PATH it also exercises the "missing binary" fallback.
        let _ = probe_reachable("lo", "127.0.0.1".parse().unwrap());
    }

    #[test]
    fn chain_names_never_exceed_the_iptables_limit() {
        let bridge = egress_bridge_name("a-very-long-egress-network-name-indeed");
        assert!(egress_nat_chain(&bridge).len() <= CHAIN_USABLE);
        assert!(egress_fwd_chain(&bridge).len() <= CHAIN_USABLE);
    }

    fn host() -> HostObjects {
        let live_bridge = egress_bridge_name("live");
        let dead_bridge = egress_bridge_name("dead");
        HostObjects {
            filter_chains: vec![
                egress_fwd_chain(&live_bridge),
                egress_fwd_chain(&dead_bridge),
                crate::wireguard::wireguard_ingress_chain("wg-dead"),
                "DOCKER-USER".into(),
                CONTAINER_INPUT_CHAIN.into(),
                "CLDR-IN-OTHER".into(),
                "ufw-before-input".into(),
            ],
            nat_chains: vec![
                egress_nat_chain(&live_bridge),
                egress_nat_chain(&dead_bridge),
                OVERLAY_SNAT_CHAIN.into(),
            ],
            policy_rules: parse_policy_rules(
                "0:\tfrom all lookup local\n\
                 5100:\tfrom all iif clex-live lookup 5001\n\
                 5100:\tfrom all iif clex-dead lookup 5002\n\
                 5100:\tfrom all iif eth9 lookup 5003\n\
                 5200:\tfrom all iif clex-dead lookup 5004\n\
                 32766:\tfrom all lookup main\n",
            ),
            bridges: vec![live_bridge, dead_bridge],
            wg_links: vec!["wg-live".into(), "wg-dead".into()],
        }
    }

    fn wanted() -> Wanted {
        Wanted {
            bridges: [egress_bridge_name("live")].into(),
            wg_ifaces: ["wg-live".to_string()].into(),
        }
    }

    #[test]
    fn the_sweep_removes_only_what_belongs_to_a_network_that_is_gone() {
        let actions = plan_sweep(&host(), &wanted());
        let dead_bridge = egress_bridge_name("dead");
        assert_eq!(
            actions,
            vec![
                SweepAction::RemoveChain {
                    table: "filter",
                    chain: egress_fwd_chain(&dead_bridge)
                },
                SweepAction::RemoveChain {
                    table: "filter",
                    chain: crate::wireguard::wireguard_ingress_chain("wg-dead")
                },
                SweepAction::RemoveChain {
                    table: "nat",
                    chain: egress_nat_chain(&dead_bridge)
                },
                SweepAction::DelRule(PolicyRule {
                    priority: 5100,
                    iif: "clex-dead".into(),
                    table: 5002
                }),
                SweepAction::FlushTable(5002),
                SweepAction::DelLink(dead_bridge),
                SweepAction::DelLink("wg-dead".into()),
            ]
        );
    }

    #[test]
    fn the_sweep_orders_chains_before_rules_before_links() {
        let actions = plan_sweep(&host(), &wanted());
        let rank = |a: &SweepAction| match a {
            SweepAction::RemoveChain { .. } => 0,
            SweepAction::DelRule(_) => 1,
            SweepAction::FlushTable(_) => 2,
            SweepAction::DelLink(_) => 3,
        };
        assert!(actions.windows(2).all(|w| rank(&w[0]) <= rank(&w[1])));
    }

    #[test]
    fn a_sweep_with_everything_wanted_does_nothing() {
        let mut wanted = wanted();
        wanted.bridges.insert(egress_bridge_name("dead"));
        wanted.wg_ifaces.insert("wg-dead".into());
        assert!(plan_sweep(&host(), &wanted).is_empty());
    }

    #[test]
    fn policy_rules_are_parsed_from_ip_rule_show() {
        let parsed = parse_policy_rules(
            "5100:\tfrom all iif clex-a lookup 5123\n0:\tfrom all lookup local\n",
        );
        assert_eq!(
            parsed,
            vec![PolicyRule {
                priority: 5100,
                iif: "clex-a".into(),
                table: 5123
            }]
        );
    }
}
