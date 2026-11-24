//! Node-to-node tunnels carrying the traffic of managed networks.
//!
//! A client that reaches a container on another node enters through any
//! implementing node, which has to hand the packet to the hosting node. Over a
//! tailnet (or any WireGuard mesh) a plain `ip route` cannot do that: the
//! mesh only carries destinations that are a peer's own address, so a packet
//! for the container's address is dropped. The hop is therefore encapsulated:
//! one IPIP tunnel per peer, wrapped in UDP (FOU) so that it is plain UDP
//! between two internal addresses (a tailnet ACL lets TCP, UDP and ICMP
//! through by default, not protocol 4). The mesh never sees the container's
//! address, only the outer addresses.
//!
//! The entry node also needs the replies to come back through the tunnel, not
//! as a packet with a foreign source on the mesh, so it source-NATs what it
//! hands over to a per-node inner address ([`inner_addr`], in 169.254.0.0/16),
//! routed to the entry node through the same tunnel by the hosting node.
//!
//! Everything here is data first; only [`ensure_tunnels`] runs `ip`.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;

use crate::error::Result;
use crate::managed::{run_ip, Hop};

/// UDP port every node listens on for the encapsulated packets.
pub const FOU_PORT: u16 = 7390;

/// Tunnel devices carry this prefix, so the daemon only lists and removes its own.
pub const TUNNEL_PREFIX: &str = "clt-";

/// IPIP over FOU adds an outer IPv4 and UDP header (28 bytes) to a 1280 byte
/// WireGuard MTU, the smallest underlay this has to fit.
pub const TUNNEL_MTU: u32 = 1252;

/// The address a node uses as the source of what it hands to another node:
/// 169.254.A.B from the last two bytes of its internal address. `None` when
/// those are zero.
pub fn inner_addr(internal: Ipv4Addr) -> Option<Ipv4Addr> {
    let [_, _, c, d] = internal.octets();
    (c != 0 || d != 0).then(|| Ipv4Addr::new(169, 254, c, d))
}

/// FNV-1a, stable across runs and builds (unlike `DefaultHasher`).
fn fnv(text: &str) -> u32 {
    text.bytes().fold(0x811c_9dc5, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

/// The device of the tunnel from `local` to `remote`. The name derives from
/// both addresses, so a node whose address changed gets a fresh device and the
/// old one is removed.
pub fn tunnel_name(local: Ipv4Addr, remote: Ipv4Addr) -> String {
    format!("{TUNNEL_PREFIX}{:08x}", fnv(&format!("{local}>{remote}")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelSpec {
    pub dev: String,
    pub local: Ipv4Addr,
    pub remote: Ipv4Addr,
}

impl TunnelSpec {
    pub fn new(local: Ipv4Addr, remote: Ipv4Addr) -> Self {
        Self {
            dev: tunnel_name(local, remote),
            local,
            remote,
        }
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

/// Start listening for the encapsulated packets (`ip fou add`).
pub fn fou_add_command() -> Vec<String> {
    strings(&["fou", "add", "port", &FOU_PORT.to_string(), "ipproto", "4"])
}

/// Whether `ip fou show` (`port 7390 ipproto 4 ...`) lists our receive port.
pub fn fou_listening(listing: &str) -> bool {
    let port = FOU_PORT.to_string();
    listing.lines().any(|l| {
        let t: Vec<&str> = l.split_whitespace().collect();
        t.windows(2).any(|w| w[0] == "port" && w[1] == port)
    })
}

/// The commands that create a tunnel, bring it up and size it.
pub fn create_commands(spec: &TunnelSpec) -> Vec<Vec<String>> {
    vec![
        strings(&[
            "link",
            "add",
            "name",
            &spec.dev,
            "type",
            "ipip",
            "local",
            &spec.local.to_string(),
            "remote",
            &spec.remote.to_string(),
            "encap",
            "fou",
            "encap-sport",
            "auto",
            "encap-dport",
            &FOU_PORT.to_string(),
        ]),
        strings(&[
            "link",
            "set",
            "dev",
            &spec.dev,
            "mtu",
            &TUNNEL_MTU.to_string(),
            "up",
        ]),
    ]
}

/// Our devices in `ip -o link show type ipip` (`7: clt-ab@NONE: <...`).
pub fn parse_tunnels(listing: &str) -> BTreeSet<String> {
    listing
        .lines()
        .filter_map(|l| {
            let name = l.split_whitespace().nth(1)?;
            let name = name.trim_end_matches(':').split('@').next()?;
            name.starts_with(TUNNEL_PREFIX).then(|| name.to_string())
        })
        .collect()
}

/// Make the host's tunnels exactly `wanted`; returns the devices that exist
/// afterwards. A tunnel that cannot be created (no `ipip` or `fou` module, no
/// permission) is left out and reported in the error list, so the caller can
/// fall back to a plain route for that peer.
pub fn ensure_tunnels(wanted: &[TunnelSpec]) -> (BTreeSet<String>, Vec<String>) {
    let mut errors = Vec::new();
    let mut have = match run_ip(&["-o", "link", "show", "type", "ipip"], false) {
        Ok(listing) => parse_tunnels(&listing),
        Err(e) => {
            errors.push(e.to_string());
            BTreeSet::new()
        }
    };
    let want: BTreeSet<&str> = wanted.iter().map(|s| s.dev.as_str()).collect();
    for stale in have.clone() {
        if !want.contains(stale.as_str()) {
            match run_ip(&["link", "del", "dev", &stale], false) {
                Ok(_) => {
                    have.remove(&stale);
                }
                Err(e) => errors.push(e.to_string()),
            }
        }
    }
    if wanted.is_empty() {
        return (have, errors);
    }
    let listening = run_ip(&["fou", "show"], false)
        .map(|l| fou_listening(&l))
        .unwrap_or(false);
    if !listening {
        if let Err(e) = run_ip_owned(&fou_add_command(), false) {
            errors.push(format!("{e} (the fou kernel module is needed)"));
            return (have, errors);
        }
    }
    for spec in wanted {
        if have.contains(&spec.dev) {
            continue;
        }
        let made = create_commands(spec)
            .iter()
            .try_for_each(|c| run_ip_owned(c, false).map(|_| ()));
        match made {
            Ok(()) => {
                have.insert(spec.dev.clone());
            }
            Err(e) => {
                let _ = run_ip(&["link", "del", "dev", &spec.dev], false);
                errors.push(format!("{e} (the ipip kernel module is needed)"));
            }
        }
    }
    (have, errors)
}

fn run_ip_owned(args: &[String], v4: bool) -> Result<String> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_ip(&refs, v4)
}

/// The hop for each container address: through the tunnel to the hosting node
/// when it exists, else the plain route via its internal address. `routes`
/// maps an address to the hosting node's internal address; `peers` is every
/// other node's `(internal, inner)` address and `tunnels` the devices that
/// exist, by remote address. Each peer's inner address is routed through its
/// tunnel too: it is where the replies to a connection it handed over go.
pub fn hops(
    routes: &BTreeMap<Ipv4Addr, Ipv4Addr>,
    peers: &[(Ipv4Addr, Ipv4Addr)],
    tunnels: &BTreeMap<Ipv4Addr, String>,
) -> BTreeMap<Ipv4Addr, Hop> {
    let mut out: BTreeMap<Ipv4Addr, Hop> = routes
        .iter()
        .map(|(dst, via)| {
            let hop = match tunnels.get(via) {
                Some(dev) => Hop::Dev(dev.clone()),
                None => Hop::Via(*via),
            };
            (*dst, hop)
        })
        .collect();
    for (internal, inner) in peers {
        if let Some(dev) = tunnels.get(internal) {
            out.insert(*inner, Hop::Dev(dev.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn the_inner_address_comes_from_the_last_two_bytes() {
        assert_eq!(inner_addr(a("100.64.0.2")), Some(a("169.254.0.2")));
        assert_eq!(inner_addr(a("10.10.3.7")), Some(a("169.254.3.7")));
        assert_eq!(inner_addr(a("10.10.0.0")), None);
    }

    #[test]
    fn the_device_name_is_stable_directional_and_short() {
        let n = tunnel_name(a("100.64.0.1"), a("100.64.0.2"));
        assert_eq!(n, tunnel_name(a("100.64.0.1"), a("100.64.0.2")));
        assert_ne!(n, tunnel_name(a("100.64.0.2"), a("100.64.0.1")));
        assert!(n.starts_with("clt-") && n.len() == 12);
    }

    #[test]
    fn a_tunnel_is_ipip_over_fou_between_the_internal_addresses() {
        let spec = TunnelSpec {
            dev: "clt-1".into(),
            local: a("100.64.0.1"),
            remote: a("100.64.0.2"),
        };
        let cmds = create_commands(&spec);
        assert_eq!(
            cmds[0].join(" "),
            "link add name clt-1 type ipip local 100.64.0.1 remote 100.64.0.2 encap fou encap-sport auto encap-dport 7390"
        );
        assert_eq!(cmds[1].join(" "), "link set dev clt-1 mtu 1252 up");
        assert_eq!(fou_add_command().join(" "), "fou add port 7390 ipproto 4");
    }

    #[test]
    fn listings_parse() {
        assert!(fou_listening("port 7390 ipproto 4 \n"));
        assert!(!fou_listening("port 5555 ipproto 4 \n"));
        assert!(!fou_listening(""));
        let listing = "7: clt-0123abcd@NONE: <POINTOPOINT,NOARP,UP> mtu 1252 qdisc noqueue\n8: tunl0@NONE: <NOARP> mtu 1480\n9: other@NONE: <X>\n";
        let got = parse_tunnels(listing);
        assert_eq!(got.into_iter().collect::<Vec<_>>(), ["clt-0123abcd"]);
    }

    #[test]
    fn hops_prefer_the_tunnel_and_route_the_peers_inner_address() {
        let routes = BTreeMap::from([
            (a("192.168.64.3"), a("100.64.0.2")),
            (a("192.168.64.4"), a("100.64.0.3")),
        ]);
        let peers = [
            (a("100.64.0.2"), a("169.254.0.2")),
            (a("100.64.0.3"), a("169.254.0.3")),
        ];
        let tunnels = BTreeMap::from([(a("100.64.0.2"), "clt-2".to_string())]);
        let got = hops(&routes, &peers, &tunnels);
        assert_eq!(got[&a("192.168.64.3")], Hop::Dev("clt-2".into()));
        // No tunnel to .3: the plain route is the fallback, and no inner route.
        assert_eq!(got[&a("192.168.64.4")], Hop::Via(a("100.64.0.3")));
        assert_eq!(got[&a("169.254.0.2")], Hop::Dev("clt-2".into()));
        assert!(!got.contains_key(&a("169.254.0.3")));
    }
}
