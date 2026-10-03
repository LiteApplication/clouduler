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

use std::net::{IpAddr, Ipv4Addr};
use std::process::Command;

use ipnet::Ipv4Net;

use crate::error::{NetError, Result};
use crate::vxlan::Overlay;

/// Linux interface names are capped at `IFNAMSIZ - 1` usable bytes.
///
/// Shared with `wireguard`, whose interface names are capped the same way.
pub(crate) const IFNAMSIZ_USABLE: usize = 15;

/// iptables chain names are capped at 28 bytes.
///
/// Shared with `wireguard`, whose ingress chains are capped the same way.
pub(crate) const CHAIN_USABLE: usize = 28;

/// Reserved for egress bridges, well away from the overlay pool
/// (`cl_net::ipam::DEFAULT_OVERLAY_POOL`) and common RFC1918 LAN ranges.
const EGRESS_POOL_BASE: u8 = 245;

/// Reserved for nat-kind bridges, its own base so a nat and an egress network
/// that hash to the same second octet still land on different subnets.
const NAT_POOL_BASE: u8 = 246;

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

fn iptables_chain(prefix: &str, network: &str) -> String {
    short_name(prefix, network, CHAIN_USABLE)
}

/// Build a `<= cap` char name from a prefix and a network name. Short names
/// pass through unchanged; long ones get truncated and a hash suffix
/// appended so two networks that truncate to the same prefix don't collide.
///
/// Shared with `wireguard`, which derives its interface names the same way.
pub(crate) fn short_name(prefix: &str, name: &str, cap: usize) -> String {
    let full = format!("{prefix}{name}");
    if full.len() <= cap {
        return full;
    }
    let suffix = format!("{:04x}", (fnv1a(name.as_bytes()) & 0xffff) as u16);
    let budget = cap - prefix.len() - suffix.len();
    let truncated: String = name.chars().take(budget).collect();
    format!("{prefix}{truncated}{suffix}")
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

/// A node-local `/24` for this network's bridge, deterministically derived
/// from its name inside the reserved `10.245.0.0/16` pool. No coordination
/// needed: this subnet is never routed to another node.
pub fn egress_subnet(network: &str) -> Ipv4Net {
    let second_octet = (fnv1a(network.as_bytes()) % 256) as u8;
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
    let second_octet = (fnv1a(network.as_bytes()) % 256) as u8;
    format!("10.{NAT_POOL_BASE}.{second_octet}.0/24")
        .parse()
        .expect("a /24 built from two bytes always parses")
}

/// The bridge's own address on its subnet: the first host address.
pub fn egress_gateway(subnet: Ipv4Net) -> Ipv4Addr {
    let base = subnet.network();
    let octets = base.octets();
    Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3] + 1)
}

/// The `iptables` invocations (as argv, pure and unit tested) that enforce
/// the kill-switch: NAT egress-network traffic out `iface` and nothing else,
/// drop anything from the bridge that doesn't leave that way.
///
/// Idempotent by construction: each network gets its own chain, flushed and
/// repopulated every call rather than appended to, and the jump into it from
/// the built-in chain is added only if not already present (checked, not
/// assumed, at apply time).
pub fn egress_iptables_plan(bridge: &str, subnet: Ipv4Net, iface: &str) -> EgressIptablesPlan {
    let nat_chain = iptables_chain("CLDR-NAT-", bridge);
    let fwd_chain = iptables_chain("CLDR-FWD-", bridge);
    let subnet = subnet.to_string();

    EgressIptablesPlan {
        nat_chain: nat_chain.clone(),
        fwd_chain: fwd_chain.clone(),
        setup: vec![
            argv(&["-t", "nat", "-N", &nat_chain]),
            argv(&["-t", "nat", "-F", &nat_chain]),
            argv(&[
                "-t",
                "nat",
                "-A",
                &nat_chain,
                "-s",
                &subnet,
                "-o",
                iface,
                "-j",
                "MASQUERADE",
            ]),
            argv(&["-N", &fwd_chain]),
            argv(&["-F", &fwd_chain]),
            argv(&["-A", &fwd_chain, "-i", bridge, "-o", iface, "-j", "ACCEPT"]),
            argv(&[
                "-A",
                &fwd_chain,
                "-i",
                iface,
                "-o",
                bridge,
                "-m",
                "state",
                "--state",
                "ESTABLISHED,RELATED",
                "-j",
                "ACCEPT",
            ]),
            // Anything from the bridge that didn't just get accepted out
            // `iface` above is dropped here: the kill-switch. No interface,
            // no route out, ever, from this chain's point of view.
            argv(&["-A", &fwd_chain, "-i", bridge, "-j", "DROP"]),
        ],
        // Existence-checked at apply time (`-C`), added only if missing:
        // these two live in shared built-in chains, so re-adding them on
        // every reconcile would duplicate rather than replace.
        jumps: vec![
            (
                argv(&["-t", "nat", "-C", "POSTROUTING", "-j", &nat_chain]),
                argv(&["-t", "nat", "-A", "POSTROUTING", "-j", &nat_chain]),
            ),
            (
                argv(&["-C", "FORWARD", "-j", &fwd_chain]),
                argv(&["-A", "FORWARD", "-j", &fwd_chain]),
            ),
        ],
    }
}

/// Source-NATs a container's overlay address when it talks to another node's
/// address on the internal network.
///
/// Overlay networks are created with docker's own masquerade off, so a packet
/// from an overlay container to a node address (where that node's reverse
/// proxy answers for every route published on the network) leaves this host
/// still carrying the overlay source. The underlay (a tailnet, say) drops it,
/// so the proxy was only reachable on the container's own node. Masquerading
/// just `overlay_pool` -> `node_addrs` makes the remote proxy see this node's
/// own internal address, which sits inside the network's subnet and passes the
/// proxy's network gate. Nothing else is NATed: an overlay container has no
/// route to anything outside the cluster except through an egress network.
///
/// A node's own address is never listed: it is delivered locally, before
/// `POSTROUTING`.
pub fn overlay_snat_plan(pool: Ipv4Net, node_addrs: &[Ipv4Addr]) -> EgressIptablesPlan {
    let nat_chain = "CLDR-OVL-SNAT".to_string();
    let pool = pool.to_string();
    let mut setup = vec![
        argv(&["-t", "nat", "-N", &nat_chain]),
        argv(&["-t", "nat", "-F", &nat_chain]),
    ];
    for addr in node_addrs {
        setup.push(argv(&[
            "-t",
            "nat",
            "-A",
            &nat_chain,
            "-s",
            &pool,
            "-d",
            &format!("{addr}/32"),
            "-j",
            "MASQUERADE",
        ]));
    }
    EgressIptablesPlan {
        nat_chain: nat_chain.clone(),
        fwd_chain: String::new(),
        setup,
        jumps: vec![(
            argv(&["-t", "nat", "-C", "POSTROUTING", "-j", &nat_chain]),
            argv(&["-t", "nat", "-A", "POSTROUTING", "-j", &nat_chain]),
        )],
    }
}

pub struct EgressIptablesPlan {
    pub nat_chain: String,
    pub fwd_chain: String,
    /// Always run, in order: create-or-ignore the two chains, flush them,
    /// repopulate them.
    pub setup: Vec<Vec<String>>,
    /// (check, add) pairs: run `check`, and only run `add` if it failed.
    pub jumps: Vec<(Vec<String>, Vec<String>)>,
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
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

/// Apply the kill-switch ruleset. `-N` on an existing chain and a failing
/// `-C` both fail non-zero for reasons that are the normal, expected path
/// here (chain already there; jump not yet added), so nothing here treats a
/// nonzero exit as fatal on its own — only a genuinely missing `iptables`
/// binary or a permission error is.
pub fn apply_egress_iptables(plan: &EgressIptablesPlan) -> Result<()> {
    for args in &plan.setup {
        let result = run_iptables(args);
        // `-N` (new chain) on a chain that already exists is exit 1, the
        // normal steady-state outcome after the first reconcile, not a real
        // failure. Everything else in `setup` (flush, append) is expected to
        // succeed every time.
        if result.is_err() && args.iter().any(|a| a == "-N") {
            continue;
        }
        result?;
    }
    for (check, add) in &plan.jumps {
        if run_iptables(check).is_err() {
            run_iptables(add)?;
        }
    }
    Ok(())
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

fn run_iptables(args: &[String]) -> Result<()> {
    let output = Command::new("iptables")
        .args(args)
        .output()
        .map_err(|e| NetError::Io {
            context: format!("iptables {}", args.join(" ")),
            source: e,
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(NetError::Io {
            context: format!(
                "iptables {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            ),
            source: std::io::Error::other("iptables exited non-zero"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_gateway_is_the_first_host_address() {
        let subnet: Ipv4Net = "10.245.7.0/24".parse().unwrap();
        assert_eq!(egress_gateway(subnet), Ipv4Addr::new(10, 245, 7, 1));
    }

    #[test]
    fn the_ruleset_only_accepts_traffic_out_the_named_interface() {
        let plan = egress_iptables_plan("clex-vpn", "10.245.7.0/24".parse().unwrap(), "wg0");
        let fwd: Vec<String> = plan
            .setup
            .iter()
            .filter(|argv| argv.contains(&"-A".to_string()) && argv.contains(&plan.fwd_chain))
            .map(|argv| argv.join(" "))
            .collect();
        assert!(fwd
            .iter()
            .any(|r| r.contains("-o wg0") && r.contains("-j ACCEPT")));
        // The drop has to come after the accept-out-wg0 rule, or it would
        // never be reached; iptables evaluates in order, and this list is
        // fed to iptables in the order it appears here.
        let drop_pos = fwd
            .iter()
            .position(|r| r.ends_with("-j DROP"))
            .expect("a drop rule");
        let accept_pos = fwd
            .iter()
            .position(|r| r.contains("-o wg0"))
            .expect("an accept rule");
        assert!(
            accept_pos < drop_pos,
            "accept-out-the-interface must be evaluated before the drop"
        );
    }

    #[test]
    fn the_masquerade_rule_scopes_to_the_subnet_and_interface() {
        let plan = egress_iptables_plan("clex-vpn", "10.245.7.0/24".parse().unwrap(), "wg0");
        let nat = plan
            .setup
            .iter()
            .find(|a| a.contains(&"MASQUERADE".to_string()))
            .expect("a masquerade rule");
        assert!(nat.iter().any(|a| a == "10.245.7.0/24"));
        assert!(nat.windows(2).any(|w| w == ["-o", "wg0"]));
    }

    #[test]
    fn the_overlay_snat_only_masquerades_the_pool_towards_the_listed_nodes() {
        let plan = overlay_snat_plan(
            "10.244.0.0/16".parse().unwrap(),
            &[Ipv4Addr::new(100, 64, 0, 1), Ipv4Addr::new(100, 64, 0, 2)],
        );
        let rules: Vec<String> = plan
            .setup
            .iter()
            .filter(|a| a.contains(&"-A".to_string()))
            .map(|a| a.join(" "))
            .collect();
        assert_eq!(rules.len(), 2);
        assert!(rules[0].contains("-s 10.244.0.0/16"));
        assert!(rules[0].contains("-d 100.64.0.1/32"));
        assert!(rules.iter().all(|r| r.ends_with("-j MASQUERADE")));
        // No address, no rule: the chain is still flushed so a removed node
        // stops being NATed.
        let empty = overlay_snat_plan("10.244.0.0/16".parse().unwrap(), &[]);
        assert!(!empty.setup.iter().any(|a| a.contains(&"-A".to_string())));
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
        let plan = egress_iptables_plan(
            &egress_bridge_name("a-very-long-egress-network-name-indeed"),
            "10.245.7.0/24".parse().unwrap(),
            "wg0",
        );
        assert!(plan.nat_chain.len() <= CHAIN_USABLE);
        assert!(plan.fwd_chain.len() <= CHAIN_USABLE);
    }
}
