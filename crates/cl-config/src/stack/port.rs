//! Port publishing: `"8080:80"`, `"127.0.0.1:8080:80/tcp"`, `"8080-8090:80-90"`
//! and the compose long form.

use std::fmt;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// Transport protocol for a published port.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Tcp,
    Udp,
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Protocol::Tcp => f.write_str("tcp"),
            Protocol::Udp => f.write_str("udp"),
        }
    }
}

impl Protocol {
    fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "tcp" => Ok(Protocol::Tcp),
            "udp" => Ok(Protocol::Udp),
            other => Err(format!("unknown protocol \"{other}\", expected tcp or udp")),
        }
    }
}

/// An inclusive port range. A single port has `start == end`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl PortRange {
    /// Number of ports covered, 1 for a single port.
    pub fn count(&self) -> u32 {
        u32::from(self.end) - u32::from(self.start) + 1
    }

    fn parse(s: &str) -> Result<Self, String> {
        match s.split_once('-') {
            Some((a, b)) => {
                let start: u16 = a.parse().map_err(|_| format!("invalid port \"{a}\""))?;
                let end: u16 = b.parse().map_err(|_| format!("invalid port \"{b}\""))?;
                if start > end {
                    return Err(format!("port range {s} has start after end"));
                }
                Ok(PortRange { start, end })
            }
            None => {
                let port: u16 = s.parse().map_err(|_| format!("invalid port \"{s}\""))?;
                Ok(PortRange { start: port, end: port })
            }
        }
    }
}

/// One published port, parsed from either the compose short string or long form.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "RawPort", into = "RawPort")]
pub struct PortMapping {
    pub host_ip: Option<IpAddr>,
    /// None when the host port is left to the engine to pick.
    pub published: Option<PortRange>,
    pub target: PortRange,
    pub protocol: Protocol,
    /// Compose/Swarm's `mode:`. Only settable through the long form; the short
    /// string syntax has no room for it and always means `Host`.
    pub mode: PublishMode,
}

/// Compose/Swarm's port publishing mode. Clouduler has no ingress mesh: a
/// published port is always bound directly on the single node running the
/// container, which is what swarm calls `host` mode. `Ingress` is accepted
/// syntax (so a stack file copied from swarm doesn't fail to parse on the
/// field alone) but rejected at the value level, since there is no mesh to
/// back it.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PublishMode {
    #[default]
    Host,
    Ingress,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum RawPort {
    Short(String),
    Long(LongPort),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LongPort {
    target: RawPortNumber,
    #[serde(default)]
    published: Option<RawPortNumber>,
    #[serde(default)]
    host_ip: Option<IpAddr>,
    #[serde(default)]
    protocol: Protocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<PublishMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum RawPortNumber {
    Int(u16),
    Str(String),
}

impl RawPortNumber {
    fn into_range(self) -> Result<PortRange, String> {
        match self {
            RawPortNumber::Int(p) => Ok(PortRange { start: p, end: p }),
            RawPortNumber::Str(s) => PortRange::parse(&s),
        }
    }
}

impl TryFrom<RawPort> for PortMapping {
    type Error = String;

    fn try_from(raw: RawPort) -> Result<Self, Self::Error> {
        match raw {
            RawPort::Short(s) => parse_short(&s),
            RawPort::Long(long) => {
                let target = long.target.into_range()?;
                let published = long.published.map(|p| p.into_range()).transpose()?;
                check_range_lengths(published.as_ref(), &target)?;
                if long.mode == Some(PublishMode::Ingress) {
                    return Err(
                        "mode: ingress is not supported, clouduler has no swarm-style ingress mesh; \
                         a published port is always bound directly on the node running the container \
                         (mode: host, the default). For a port forwarded over the overlay to whichever \
                         node currently runs the container, use x-clouduler.expose instead"
                            .to_string(),
                    );
                }
                Ok(PortMapping {
                    host_ip: long.host_ip,
                    published,
                    target,
                    protocol: long.protocol,
                    mode: long.mode.unwrap_or_default(),
                })
            }
        }
    }
}

impl From<PortMapping> for RawPort {
    fn from(p: PortMapping) -> Self {
        RawPort::Long(LongPort {
            target: RawPortNumber::Str(format!("{}-{}", p.target.start, p.target.end)),
            published: p
                .published
                .map(|r| RawPortNumber::Str(format!("{}-{}", r.start, r.end))),
            host_ip: p.host_ip,
            protocol: p.protocol,
            // Host is the implicit default (and only supported value), so it
            // round trips as absent rather than spelling out `mode: host`.
            mode: None,
        })
    }
}

fn check_range_lengths(published: Option<&PortRange>, target: &PortRange) -> Result<(), String> {
    if let Some(published) = published {
        if published.count() > 1 && target.count() > 1 && published.count() != target.count() {
            return Err(format!(
                "published range has {} ports but target range has {}",
                published.count(),
                target.count()
            ));
        }
    }
    Ok(())
}

fn parse_short(raw: &str) -> Result<PortMapping, String> {
    let (main, protocol) = match raw.rsplit_once('/') {
        Some((m, p)) => (m, Protocol::parse(p)?),
        None => (raw, Protocol::Tcp),
    };

    let parts: Vec<&str> = main.split(':').collect();
    let (host_ip, published, target) = match parts.as_slice() {
        [target] => (None, None, PortRange::parse(target)?),
        [published, target] => (None, Some(PortRange::parse(published)?), PortRange::parse(target)?),
        [ip, published, target] => {
            let ip: IpAddr = ip.parse().map_err(|_| format!("invalid host ip \"{ip}\""))?;
            (Some(ip), Some(PortRange::parse(published)?), PortRange::parse(target)?)
        }
        _ => return Err(format!("invalid port syntax \"{raw}\"")),
    };

    check_range_lengths(published.as_ref(), &target)?;

    Ok(PortMapping { host_ip, published, target, protocol, mode: PublishMode::default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short(s: &str) -> PortMapping {
        serde_yaml::from_str::<PortMapping>(&format!("\"{s}\"")).unwrap()
    }

    #[test]
    fn plain_target_only() {
        let p = short("80");
        assert_eq!(p.host_ip, None);
        assert_eq!(p.published, None);
        assert_eq!(p.target, PortRange { start: 80, end: 80 });
        assert_eq!(p.protocol, Protocol::Tcp);
    }

    #[test]
    fn published_and_target() {
        let p = short("8080:80");
        assert_eq!(p.published, Some(PortRange { start: 8080, end: 8080 }));
        assert_eq!(p.target, PortRange { start: 80, end: 80 });
    }

    #[test]
    fn host_ip_published_target_protocol() {
        let p = short("127.0.0.1:8080:80/tcp");
        assert_eq!(p.host_ip, Some("127.0.0.1".parse().unwrap()));
        assert_eq!(p.published, Some(PortRange { start: 8080, end: 8080 }));
        assert_eq!(p.target, PortRange { start: 80, end: 80 });
        assert_eq!(p.protocol, Protocol::Tcp);
    }

    #[test]
    fn port_ranges() {
        let p = short("8080-8090:80-90");
        assert_eq!(p.published, Some(PortRange { start: 8080, end: 8090 }));
        assert_eq!(p.target, PortRange { start: 80, end: 90 });
    }

    #[test]
    fn udp_protocol() {
        let p = short("53:53/udp");
        assert_eq!(p.protocol, Protocol::Udp);
    }

    #[test]
    fn mismatched_range_lengths_rejected() {
        let err = serde_yaml::from_str::<PortMapping>("\"8080-8090:80-95\"").unwrap_err();
        assert!(err.to_string().contains("but target range has"));
    }

    #[test]
    fn mode_host_is_accepted_and_is_the_default() {
        let p: PortMapping = serde_yaml::from_str("target: 80\nmode: host\n").unwrap();
        assert_eq!(p.mode, PublishMode::Host);

        let p = short("80");
        assert_eq!(p.mode, PublishMode::Host);
    }

    #[test]
    fn mode_ingress_is_rejected() {
        let err = serde_yaml::from_str::<PortMapping>("target: 80\nmode: ingress\n").unwrap_err();
        assert!(err.to_string().contains("ingress mesh"), "{err}");
        assert!(err.to_string().contains("expose"), "{err}");
    }

    #[test]
    fn mode_round_trips_as_absent() {
        let p: PortMapping = serde_yaml::from_str("target: 80\nmode: host\n").unwrap();
        let encoded = serde_yaml::to_string(&p).unwrap();
        assert!(!encoded.contains("mode"), "{encoded}");
    }

    #[test]
    fn long_form() {
        let p: PortMapping = serde_yaml::from_str(
            r#"
target: 80
published: "8080"
protocol: udp
"#,
        )
        .unwrap();
        assert_eq!(p.target, PortRange { start: 80, end: 80 });
        assert_eq!(p.published, Some(PortRange { start: 8080, end: 8080 }));
        assert_eq!(p.protocol, Protocol::Udp);
    }
}
