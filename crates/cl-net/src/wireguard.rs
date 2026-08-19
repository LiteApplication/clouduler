//! WireGuard-kind networks: clouduler brings up the tunnel itself from an
//! imported config, instead of expecting the admin to have set one up out of
//! band the way a plain `Egress` network does.
//!
//! Once the interface named by [`wireguard_iface_name`] is up, it is just a
//! host interface as far as `egress` is concerned: the bridge, kill-switch
//! and docker network machinery in [`crate::egress`] apply unchanged, routing
//! egress-network traffic out this interface exactly like it would out an
//! admin-managed one.
//!
//! `rtnetlink` (already used by `vxlan`) can create a bare `wireguard`-type
//! link, but has no support for the WireGuard generic-netlink family that
//! sets keys and peers -- that only exists in `wg`(8), so, consistent with
//! `egress`'s choice to shell out to `iptables` rather than hand-roll it over
//! netlink, key and peer configuration here shells out to `ip` and `wg` too.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::IpAddr;
use std::process::Command;

use cl_config::Protocol;
use ipnet::IpNet;

use crate::egress::{short_name, CHAIN_USABLE, IFNAMSIZ_USABLE};
use crate::error::{NetError, Result};

/// Deterministic interface name for a wireguard network's tunnel.
pub fn wireguard_iface_name(network: &str) -> String {
    short_name("wg-", network, IFNAMSIZ_USABLE)
}

/// A parsed wg-quick style config: the `[Interface]` section plus one or more
/// `[Peer]` sections. Fields wg-quick itself would apply outside the kernel
/// (`DNS`, `Table`, `PostUp`/`PostDown`, ...) are not modeled: clouduler only
/// needs what `wg setconf` and `ip addr` need.
#[derive(Debug, Clone, PartialEq)]
pub struct WireguardConfig {
    pub private_key: String,
    pub address: IpNet,
    pub listen_port: Option<u16>,
    pub peers: Vec<WireguardPeer>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WireguardPeer {
    pub public_key: String,
    pub endpoint: Option<String>,
    pub allowed_ips: Vec<IpNet>,
    pub preshared_key: Option<String>,
    pub persistent_keepalive: Option<u16>,
}

/// Parse a wg-quick `.conf`. Rejects anything missing `PrivateKey` or
/// `Address` on the interface, or a peer missing `PublicKey` -- those three
/// are the minimum a tunnel needs to come up at all.
pub fn parse_wg_quick(text: &str) -> Result<WireguardConfig> {
    let mut private_key = None;
    let mut address = None;
    let mut listen_port = None;
    let mut peers = Vec::new();
    let mut current: Option<BTreeMap<String, String>> = None;
    let mut in_peer = false;

    let flush_peer = |current: &mut Option<BTreeMap<String, String>>, peers: &mut Vec<WireguardPeer>| -> Result<()> {
        let Some(fields) = current.take() else { return Ok(()) };
        let public_key = fields
            .get("publickey")
            .ok_or_else(|| parse_err("a [Peer] section is missing PublicKey"))?
            .clone();
        let allowed_ips = match fields.get("allowedips") {
            Some(raw) => raw
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.parse::<IpNet>().map_err(|_| parse_err(&format!("invalid AllowedIPs entry {s}"))))
                .collect::<Result<Vec<_>>>()?,
            None => Vec::new(),
        };
        peers.push(WireguardPeer {
            public_key,
            endpoint: fields.get("endpoint").cloned(),
            allowed_ips,
            preshared_key: fields.get("presharedkey").cloned(),
            persistent_keepalive: fields.get("persistentkeepalive").and_then(|s| s.parse().ok()),
        });
        Ok(())
    };

    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.eq_ignore_ascii_case("[interface]") {
            flush_peer(&mut current, &mut peers)?;
            in_peer = false;
            current = None;
            continue;
        }
        if line.eq_ignore_ascii_case("[peer]") {
            flush_peer(&mut current, &mut peers)?;
            in_peer = true;
            current = Some(BTreeMap::new());
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(parse_err(&format!("malformed line: {raw_line}")));
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_string();

        if in_peer {
            current.get_or_insert_with(BTreeMap::new).insert(key, value);
            continue;
        }
        match key.as_str() {
            "privatekey" => private_key = Some(value),
            "address" => {
                address = Some(
                    value
                        .split(',')
                        .next()
                        .unwrap_or(&value)
                        .trim()
                        .parse::<IpNet>()
                        .map_err(|_| parse_err(&format!("invalid Address {value}")))?,
                )
            }
            "listenport" => listen_port = value.parse().ok(),
            // DNS, MTU, Table, PostUp/PostDown and friends: wg-quick-only,
            // clouduler manages routing itself, so these are ignored rather
            // than rejected -- a config pasted straight from a VPN provider
            // usually carries them.
            _ => {}
        }
    }
    flush_peer(&mut current, &mut peers)?;

    let private_key = private_key.ok_or_else(|| parse_err("missing PrivateKey in [Interface]"))?;
    let address = address.ok_or_else(|| parse_err("missing Address in [Interface]"))?;
    if peers.is_empty() {
        return Err(parse_err("no [Peer] section found"));
    }

    Ok(WireguardConfig { private_key, address, listen_port, peers })
}

fn parse_err(msg: &str) -> NetError {
    NetError::Io {
        context: "parsing wireguard config".to_string(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string()),
    }
}

/// Bring up `name` as a wireguard interface configured per `cfg`. Idempotent:
/// safe to call every reconcile tick, whether or not the interface already
/// exists with the right settings.
pub fn ensure_wireguard_interface(name: &str, cfg: &WireguardConfig) -> Result<()> {
    let add = Command::new("ip")
        .args(["link", "add", name, "type", "wireguard"])
        .output()
        .map_err(|e| io_err(format!("ip link add {name}"), e))?;
    // Exit code from "already exists" is the normal steady-state case after
    // the first reconcile, not a real failure.
    if !add.status.success() && !String::from_utf8_lossy(&add.stderr).contains("File exists") {
        return Err(cmd_err("ip link add", &add.stderr));
    }

    let tmp = write_tempfile(&wg_setconf_body(cfg))?;
    let result = run_ok("wg", &["setconf", name, tmp.to_str().expect("temp path is always utf8")]);
    let _ = std::fs::remove_file(&tmp);
    result?;

    run_ok("ip", &["address", "replace", &cfg.address.to_string(), "dev", name])?;
    run_ok("ip", &["link", "set", name, "up"])?;
    Ok(())
}

/// Tear the interface down. Not an error if it is already gone.
pub fn teardown_wireguard_interface(name: &str) -> Result<()> {
    let out = Command::new("ip")
        .args(["link", "del", name])
        .output()
        .map_err(|e| io_err(format!("ip link del {name}"), e))?;
    if !out.status.success() && !String::from_utf8_lossy(&out.stderr).contains("Cannot find device") {
        return Err(cmd_err("ip link del", &out.stderr));
    }
    Ok(())
}

/// The subset of a wg-quick config that `wg setconf` accepts: `Address`,
/// `DNS`, `MTU`, `Table` and the `PostUp`/`PostDown` family are wg-quick-only
/// and `wg setconf` rejects a file containing them.
fn wg_setconf_body(cfg: &WireguardConfig) -> String {
    let mut out = String::from("[Interface]\n");
    out.push_str(&format!("PrivateKey={}\n", cfg.private_key));
    if let Some(port) = cfg.listen_port {
        out.push_str(&format!("ListenPort={port}\n"));
    }
    for peer in &cfg.peers {
        out.push_str("[Peer]\n");
        out.push_str(&format!("PublicKey={}\n", peer.public_key));
        if let Some(psk) = &peer.preshared_key {
            out.push_str(&format!("PresharedKey={psk}\n"));
        }
        if let Some(endpoint) = &peer.endpoint {
            out.push_str(&format!("Endpoint={endpoint}\n"));
        }
        if !peer.allowed_ips.is_empty() {
            let ips: Vec<String> = peer.allowed_ips.iter().map(|n| n.to_string()).collect();
            out.push_str(&format!("AllowedIPs={}\n", ips.join(",")));
        }
        if let Some(keepalive) = peer.persistent_keepalive {
            out.push_str(&format!("PersistentKeepalive={keepalive}\n"));
        }
    }
    out
}

/// A private-key-bearing config never touches disk anywhere but here, and
/// only for the moment `wg setconf` needs to read it; the file is
/// user-readable-only and the caller removes it immediately after use.
fn write_tempfile(content: &str) -> Result<std::path::PathBuf> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = std::env::temp_dir().join(format!("cl-wg-{}.conf", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| io_err("creating wireguard temp file".to_string(), e))?;
    file.write_all(content.as_bytes()).map_err(|e| io_err("writing wireguard temp file".to_string(), e))?;
    Ok(path)
}

fn run_ok(bin: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(bin).args(args).output().map_err(|e| io_err(format!("{bin} {}", args.join(" ")), e))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(cmd_err(bin, &out.stderr))
    }
}

fn cmd_err(context: &str, stderr: &[u8]) -> NetError {
    NetError::Io {
        context: format!("{context}: {}", String::from_utf8_lossy(stderr)),
        source: std::io::Error::other("command exited non-zero"),
    }
}

fn io_err(context: String, source: std::io::Error) -> NetError {
    NetError::Io { context, source }
}

/// One allowed inbound port on a wireguard network's tunnel, forwarded to a
/// port on this node -- normally a docker container's already-published
/// host port.
#[derive(Debug, Clone, PartialEq)]
pub struct IngressRule {
    pub port: u16,
    pub protocol: Protocol,
    pub target_port: u16,
}

/// The `iptables` invocations that let specific ports in through `iface` and
/// on to `target_addr` -- the node's own address, so an already-published
/// docker `-p` bind (which listens on `0.0.0.0`) answers there too, with no
/// need for `route_localnet` the way a loopback target would.
///
/// Scoped to `-i <iface>` throughout: never opens these ports on the bridge,
/// the internal network, or anywhere else.
pub fn wireguard_ingress_iptables_plan(iface: &str, target_addr: IpAddr, rules: &[IngressRule]) -> IngressIptablesPlan {
    let chain = short_name("CLDR-WGIN-", iface, CHAIN_USABLE);
    let mut setup = vec![argv(&["-N", &chain]), argv(&["-t", "nat", "-N", &chain]), argv(&["-F", &chain]), argv(&["-t", "nat", "-F", &chain])];

    for rule in rules {
        let proto = rule.protocol.to_string();
        let port = rule.port.to_string();
        setup.push(argv(&["-A", &chain, "-i", iface, "-p", &proto, "--dport", &port, "-j", "ACCEPT"]));
        if rule.target_port != rule.port {
            let dest = format!("{target_addr}:{}", rule.target_port);
            setup.push(argv(&["-t", "nat", "-A", &chain, "-i", iface, "-p", &proto, "--dport", &port, "-j", "DNAT", "--to-destination", &dest]));
        }
    }

    IngressIptablesPlan {
        chain: chain.clone(),
        setup,
        jumps: vec![
            (argv(&["-C", "INPUT", "-j", &chain]), argv(&["-A", "INPUT", "-j", &chain])),
            (argv(&["-t", "nat", "-C", "PREROUTING", "-j", &chain]), argv(&["-t", "nat", "-A", "PREROUTING", "-j", &chain])),
        ],
    }
}

pub struct IngressIptablesPlan {
    pub chain: String,
    pub setup: Vec<Vec<String>>,
    pub jumps: Vec<(Vec<String>, Vec<String>)>,
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Apply an ingress ruleset the same way `egress::apply_egress_iptables`
/// applies its kill-switch: `-N` on an existing chain is the normal
/// steady-state, everything else is expected to succeed.
pub fn apply_wireguard_ingress_iptables(plan: &IngressIptablesPlan) -> Result<()> {
    for args in &plan.setup {
        let result = run_iptables(args);
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

fn run_iptables(args: &[String]) -> Result<()> {
    let output = Command::new("iptables").args(args).output().map_err(|e| io_err(format!("iptables {}", args.join(" ")), e))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(cmd_err("iptables", &output.stderr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[Interface]
PrivateKey = cHJpdmF0ZWtleWJhc2U2NGV4YW1wbGU=
Address = 10.6.0.2/32
DNS = 10.6.0.1
ListenPort = 51820

[Peer]
PublicKey = cHVibGlja2V5YmFzZTY0ZXhhbXBsZQ==
Endpoint = vpn.example.com:51820
AllowedIPs = 0.0.0.0/0, ::/0
PersistentKeepalive = 25
"#;

    #[test]
    fn parses_a_wg_quick_config() {
        let cfg = parse_wg_quick(SAMPLE).expect("valid");
        assert_eq!(cfg.private_key, "cHJpdmF0ZWtleWJhc2U2NGV4YW1wbGU=");
        assert_eq!(cfg.address.to_string(), "10.6.0.2/32");
        assert_eq!(cfg.listen_port, Some(51820));
        assert_eq!(cfg.peers.len(), 1);
        let peer = &cfg.peers[0];
        assert_eq!(peer.public_key, "cHVibGlja2V5YmFzZTY0ZXhhbXBsZQ==");
        assert_eq!(peer.endpoint.as_deref(), Some("vpn.example.com:51820"));
        assert_eq!(peer.allowed_ips.len(), 2);
        assert_eq!(peer.persistent_keepalive, Some(25));
    }

    #[test]
    fn missing_private_key_is_rejected() {
        let bad = SAMPLE.replace("PrivateKey = cHJpdmF0ZWtleWJhc2U2NGV4YW1wbGU=\n", "");
        let err = parse_wg_quick(&bad).unwrap_err();
        assert!(err.to_string().contains("PrivateKey"));
    }

    #[test]
    fn missing_peer_is_rejected() {
        let bad = SAMPLE.split("[Peer]").next().unwrap();
        let err = parse_wg_quick(bad).unwrap_err();
        assert!(err.to_string().contains("[Peer]"));
    }

    #[test]
    fn peer_missing_public_key_is_rejected() {
        let bad = SAMPLE.replace("PublicKey = cHVibGlja2V5YmFzZTY0ZXhhbXBsZQ==\n", "");
        let err = parse_wg_quick(&bad).unwrap_err();
        assert!(err.to_string().contains("PublicKey"));
    }

    #[test]
    fn setconf_body_omits_wg_quick_only_keys() {
        let cfg = parse_wg_quick(SAMPLE).unwrap();
        let body = wg_setconf_body(&cfg);
        assert!(!body.contains("DNS"));
        assert!(!body.contains("Address"));
        assert!(body.contains("PrivateKey="));
        assert!(body.contains("AllowedIPs=0.0.0.0/0,::/0"));
    }

    #[test]
    fn iface_name_is_deterministic_and_capped() {
        let name = wireguard_iface_name("a-very-long-wireguard-network-name");
        assert!(name.len() <= IFNAMSIZ_USABLE);
        assert!(name.starts_with("wg-"));
        assert_eq!(wireguard_iface_name("vpn"), "wg-vpn");
    }

    #[test]
    fn ingress_plan_scopes_rules_to_the_tunnel_interface() {
        let rules = [IngressRule { port: 2222, protocol: Protocol::Tcp, target_port: 22 }];
        let plan = wireguard_ingress_iptables_plan("wg-vpn", "10.10.0.5".parse().unwrap(), &rules);
        let accept = plan.setup.iter().find(|a| a.contains(&"ACCEPT".to_string())).expect("an accept rule");
        assert!(accept.windows(2).any(|w| w == ["-i", "wg-vpn"]));
        let dnat = plan.setup.iter().find(|a| a.contains(&"DNAT".to_string())).expect("a dnat rule");
        assert!(dnat.iter().any(|a| a == "10.10.0.5:22"));
    }

    #[test]
    fn ingress_plan_skips_dnat_when_ports_match() {
        let rules = [IngressRule { port: 22, protocol: Protocol::Tcp, target_port: 22 }];
        let plan = wireguard_ingress_iptables_plan("wg-vpn", "10.10.0.5".parse().unwrap(), &rules);
        assert!(!plan.setup.iter().any(|a| a.contains(&"DNAT".to_string())));
    }
}
