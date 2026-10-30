//! The `allowed` list of a stack network: what a stack's containers may reach
//! through that network.
//!
//! One entry is a string so a list stays readable in a compose file:
//!
//! * `auth.example.org`: a hostname with no port. Enforced by the reverse
//!   proxy, which answers 403 to any other host of that network. `*.example.org`
//!   matches exactly one extra label, like a proxy route host.
//! * `db.example.org:5432`, `db.example.org:8000-8100/udp`: a host, a port (or
//!   range) and a protocol (`tcp` by default). Enforced by the host firewall,
//!   which resolves the name and accepts only that destination.
//! * An IPv4 literal takes either form.
//!
//! On a wan (`egress`, `wireguard` or `nat`) network every entry is enforced
//! by the firewall: a hostname (or `*.suffix`) is allowed on the addresses
//! the stack's own DNS lookups of it return, an entry without a port meaning
//! every port and protocol. See `docs/stacks.md`.

use std::fmt;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use super::port::{PortRange, Protocol};

/// Longest hostname DNS allows.
const MAX_HOST_LEN: usize = 253;

/// Who an entry names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AllowedHost {
    /// A hostname, lowercased with no trailing dot.
    Name(String),
    /// `*.suffix`: stores `suffix`. One extra label, never the apex.
    Wildcard(String),
    Ip(Ipv4Addr),
}

/// The port part of an entry that is enforced by the firewall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AllowedPorts {
    pub protocol: Protocol,
    pub range: PortRange,
}

/// One parsed `allowed` entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AllowedTarget {
    pub host: AllowedHost,
    /// `None` is a hostname-only entry, checked by the proxy.
    pub ports: Option<AllowedPorts>,
}

impl AllowedTarget {
    /// A wildcard with a port. Only meaningful on a wan (egress, wireguard or
    /// nat) network, where the firewall enforces it on the addresses the
    /// name resolves to; on any other network a wildcard is a proxy host
    /// pattern and cannot take a port.
    pub fn is_wildcard_with_port(&self) -> bool {
        matches!(self.host, AllowedHost::Wildcard(_)) && self.ports.is_some()
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("an allowed entry cannot be empty".into());
        }
        let (main, protocol) = match raw.rsplit_once('/') {
            Some((m, p)) => (m, Some(Protocol::parse(p)?)),
            None => (raw, None),
        };
        let (host, ports) = match main.rsplit_once(':') {
            Some((h, p)) => (h, Some(PortRange::parse(p)?)),
            None => (main, None),
        };
        if protocol.is_some() && ports.is_none() {
            return Err(format!(
                "\"{raw}\": a protocol needs a port, write host:port/{}",
                protocol.unwrap_or_default()
            ));
        }
        let host = parse_host(host)?;
        Ok(AllowedTarget {
            host,
            ports: ports.map(|range| AllowedPorts {
                protocol: protocol.unwrap_or_default(),
                range,
            }),
        })
    }
}

fn parse_host(host: &str) -> Result<AllowedHost, String> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Err("an allowed entry needs a host".into());
    }
    if host.len() > MAX_HOST_LEN {
        return Err(format!("host \"{host}\" is too long"));
    }
    if let Some(suffix) = host.strip_prefix("*.") {
        check_name(suffix)?;
        return Ok(AllowedHost::Wildcard(suffix.to_string()));
    }
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(AllowedHost::Ip(ip));
    }
    check_name(&host)?;
    Ok(AllowedHost::Name(host))
}

fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        });
    if ok {
        Ok(())
    } else {
        Err(format!("\"{name}\" is not a valid hostname"))
    }
}

/// Deserializes a network's `allowed` key. A key with no value (`allowed:`,
/// typically every entry commented out) is an error, not "no restriction":
/// silently opening a stack up is the worst way to read an empty list.
pub(super) fn list<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Vec<AllowedTarget>>, D::Error> {
    const HINT: &str =
        "allowed must be a list (write `allowed: []` to deny everything, or drop the key)";
    match Option::<Vec<AllowedTarget>>::deserialize(d) {
        Ok(Some(list)) => Ok(Some(list)),
        Ok(None) => Err(serde::de::Error::custom(HINT)),
        Err(e) => Err(serde::de::Error::custom(format!("{HINT}: {e}"))),
    }
}

impl fmt::Display for AllowedTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            AllowedHost::Name(n) => f.write_str(n)?,
            AllowedHost::Wildcard(s) => write!(f, "*.{s}")?,
            AllowedHost::Ip(ip) => write!(f, "{ip}")?,
        }
        if let Some(p) = &self.ports {
            if p.range.start == p.range.end {
                write!(f, ":{}", p.range.start)?;
            } else {
                write!(f, ":{}-{}", p.range.start, p.range.end)?;
            }
            if p.protocol != Protocol::Tcp {
                write!(f, "/{}", p.protocol)?;
            }
        }
        Ok(())
    }
}

impl TryFrom<String> for AllowedTarget {
    type Error = String;
    fn try_from(raw: String) -> Result<Self, String> {
        AllowedTarget::parse(&raw)
    }
}

impl From<AllowedTarget> for String {
    fn from(t: AllowedTarget) -> String {
        t.to_string()
    }
}

impl Serialize for AllowedTarget {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for AllowedTarget {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        AllowedTarget::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> AllowedTarget {
        AllowedTarget::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn a_bare_host_is_a_proxy_entry() {
        let t = ok("Auth.Example.org.");
        assert_eq!(t.host, AllowedHost::Name("auth.example.org".into()));
        assert_eq!(t.ports, None);
    }

    #[test]
    fn a_port_makes_it_a_firewall_entry_defaulting_to_tcp() {
        let t = ok("service.home.liteapp.fr:22");
        let p = t.ports.unwrap();
        assert_eq!(p.protocol, Protocol::Tcp);
        assert_eq!(p.range, PortRange { start: 22, end: 22 });
    }

    #[test]
    fn ranges_and_protocols() {
        let p = ok("db.example.org:8000-8100/udp").ports.unwrap();
        assert_eq!(p.protocol, Protocol::Udp);
        assert_eq!(
            p.range,
            PortRange {
                start: 8000,
                end: 8100
            }
        );
    }

    #[test]
    fn ip_literals_take_both_forms() {
        assert_eq!(
            ok("10.0.0.5").host,
            AllowedHost::Ip("10.0.0.5".parse().unwrap())
        );
        assert!(ok("10.0.0.5:22").ports.is_some());
    }

    #[test]
    fn wildcards_parse_with_or_without_a_port() {
        assert_eq!(
            ok("*.Example.org").host,
            AllowedHost::Wildcard("example.org".into())
        );
        assert!(ok("*.example.org:443").is_wildcard_with_port());
        assert!(!ok("*.example.org").is_wildcard_with_port());
    }

    #[test]
    fn malformed_entries_are_rejected() {
        for bad in [
            "",
            " ",
            ":22",
            "host:",
            "host:0x10",
            "host:70000",
            "host:90-80",
            "host/udp",
            "host:22/sctp",
            "bad host",
            "a..b",
            "-x.example.org",
            "x-.example.org",
            "a.-b.example.org:22",
            "*.",
            "*",
            "http://example.org",
        ] {
            assert!(AllowedTarget::parse(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn display_round_trips() {
        for s in [
            "auth.example.org",
            "*.example.org",
            "10.0.0.5",
            "db.example.org:5432",
            "db.example.org:8000-8100/udp",
        ] {
            assert_eq!(ok(s).to_string(), s);
            assert_eq!(ok(&ok(s).to_string()), ok(s));
        }
        // The default protocol is not spelled out.
        assert_eq!(ok("h.example.org:22/tcp").to_string(), "h.example.org:22");
    }

    #[test]
    fn yaml_round_trip() {
        let t: AllowedTarget = serde_yaml::from_str("\"svc.example.org:22\"").unwrap();
        assert_eq!(
            serde_yaml::to_string(&t).unwrap().trim(),
            "svc.example.org:22"
        );
        assert!(serde_yaml::from_str::<AllowedTarget>("\"bad host\"").is_err());
    }
}

#[cfg(test)]
mod list_tests {
    use super::super::StackNetwork;

    #[test]
    fn a_key_with_no_value_is_refused_not_read_as_unrestricted() {
        for yaml in ["allowed:\n", "allowed: null\n", "allowed: ~\n"] {
            let err = serde_yaml::from_str::<StackNetwork>(yaml).unwrap_err();
            assert!(
                err.to_string().contains("allowed must be a list"),
                "{yaml}: {err}"
            );
        }
        assert_eq!(
            serde_yaml::from_str::<StackNetwork>("{}").unwrap().allowed,
            None
        );
        assert_eq!(
            serde_yaml::from_str::<StackNetwork>("allowed: []")
                .unwrap()
                .allowed,
            Some(vec![])
        );
    }
}
