//! Per-caller host allow lists: what a stack that declared `allowed` on a
//! network may ask the proxy for.
//!
//! The caller is identified by its TCP peer address, never a header, and the
//! table maps an overlay subnet to the stack that owns it. A request is
//! checked against the list its stack declared for the network the matched
//! route is published on; a route on a network the stack declared nothing for
//! is unaffected.
//!
//! The cost is built to disappear when unused: with no restricted stack the
//! table is empty and a request pays one atomic load and one emptiness check.
//! Otherwise it is a scan of the (few) restricted subnets, and a hash lookup
//! only for a peer that is in one.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Arc;

use arc_swap::ArcSwap;
use ipnet::Ipv4Net;

/// The hosts one network's `allowed` list names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostAllow {
    exact: HashSet<String>,
    /// Bases of `*.base` entries. A wildcard matches exactly one extra label,
    /// the same rule a wildcard route uses.
    wildcards: Vec<String>,
}

impl HostAllow {
    /// Build from entries as written (`auth.example.org`, `*.example.org`).
    pub fn new<I, S>(hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut allow = HostAllow::default();
        for host in hosts {
            let host = host.as_ref().trim_end_matches('.').to_ascii_lowercase();
            match host.strip_prefix("*.") {
                Some(base) => allow.wildcards.push(base.to_string()),
                None => {
                    allow.exact.insert(host);
                }
            }
        }
        allow
    }

    /// Is `host` (a `Host` header value, port allowed) on the list?
    pub fn permits(&self, host: &str) -> bool {
        let host = strip_port(host).trim_end_matches('.');
        if host.bytes().any(|b| b.is_ascii_uppercase()) {
            return self.permits_lower(&host.to_ascii_lowercase());
        }
        self.permits_lower(host)
    }

    fn permits_lower(&self, host: &str) -> bool {
        self.exact.contains(host)
            || self
                .wildcards
                .iter()
                .any(|base| crate::route::wildcard_matches(base, host))
    }
}

fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host.split(']').next().map_or(host, |h| &h[1..]);
    }
    host.split(':').next().unwrap_or(host)
}

/// One stack's allow lists, by the network each was declared on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StackPolicy {
    pub stack: String,
    pub networks: HashMap<String, HostAllow>,
    /// The hostnames the stack's own services publish, by the network each
    /// route is on. Always permitted there, so a stack can call its own
    /// public address without listing it.
    pub own_routes: HashMap<String, HostAllow>,
}

/// Why a request was refused, for the 403 body and the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    pub stack: String,
    pub network: String,
    pub host: String,
}

impl StackPolicy {
    fn check(&self, route_network: &str, host: &str) -> Option<Denied> {
        let allow = self.networks.get(route_network)?;
        if allow.permits(host)
            || self
                .own_routes
                .get(route_network)
                .is_some_and(|own| own.permits(host))
        {
            return None;
        }
        Some(Denied {
            stack: self.stack.clone(),
            network: route_network.to_string(),
            host: strip_port(host).to_string(),
        })
    }
}

/// Overlay subnet -> the policy of the stack living on it. Swapped whole.
#[derive(Default)]
pub struct PeerPolicies {
    entries: ArcSwap<Vec<(Ipv4Net, StackPolicy)>>,
}

impl PeerPolicies {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replace(&self, entries: Vec<(Ipv4Net, StackPolicy)>) {
        self.entries.store(Arc::new(entries));
    }

    pub fn is_empty(&self) -> bool {
        self.entries.load().is_empty()
    }

    /// Refuse `host` on a route published on `route_network` when the peer's
    /// stack restricted that network and did not list the host.
    pub fn check(&self, peer: IpAddr, route_network: Option<&str>, host: &str) -> Option<Denied> {
        let entries = self.entries.load();
        if entries.is_empty() {
            return None;
        }
        let route_network = route_network?;
        let IpAddr::V4(ip) = peer.to_canonical() else {
            return None;
        };
        let (_, policy) = entries.iter().find(|(subnet, _)| subnet.contains(&ip))?;
        policy.check(route_network, host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policies() -> PeerPolicies {
        let p = PeerPolicies::new();
        p.replace(vec![
            (
                "10.244.1.0/24".parse().unwrap(),
                StackPolicy {
                    stack: "blog".into(),
                    networks: HashMap::from([
                        (
                            "tailscale".to_string(),
                            HostAllow::new(["cloud.home.example.org", "*.apps.example.org"]),
                        ),
                        ("wan".to_string(), HostAllow::new(["auth.example.org"])),
                        ("sealed".to_string(), HostAllow::new(Vec::<String>::new())),
                    ]),
                    own_routes: HashMap::from([
                        (
                            "tailscale".to_string(),
                            HostAllow::new(["blog.home.example.org"]),
                        ),
                        (
                            "sealed".to_string(),
                            HostAllow::new(["me.sealed.example.org"]),
                        ),
                    ]),
                },
            ),
            (
                "10.244.2.0/24".parse().unwrap(),
                StackPolicy {
                    stack: "other".into(),
                    networks: HashMap::from([(
                        "wan".to_string(),
                        HostAllow::new(["x.example.org"]),
                    )]),
                    own_routes: HashMap::new(),
                },
            ),
        ]);
        p
    }

    fn peer(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn exact_wildcard_and_port_and_case() {
        let a = HostAllow::new(["Auth.Example.org.", "*.apps.example.org"]);
        assert!(a.permits("auth.example.org"));
        assert!(a.permits("AUTH.example.org:8443"));
        assert!(a.permits("auth.example.org."));
        assert!(a.permits("x.apps.example.org"));
        assert!(!a.permits("apps.example.org"));
        assert!(!a.permits("a.b.apps.example.org"));
        assert!(!a.permits("evil-auth.example.org"));
        assert!(!a.permits("auth.example.org.evil.com"));
        assert!(!a.permits(""));
    }

    #[test]
    fn a_stacks_own_routes_pass_without_being_listed() {
        let p = policies();
        let blog = peer("10.244.1.9");
        // Not on the tailscale list, but the stack publishes it there itself.
        assert_eq!(
            p.check(blog, Some("tailscale"), "blog.home.example.org"),
            None
        );
        assert_eq!(
            p.check(blog, Some("tailscale"), "BLOG.home.example.org:443"),
            None
        );
        // Even on a network whose list is empty.
        assert_eq!(p.check(blog, Some("sealed"), "me.sealed.example.org"), None);
        // Only on the network the route is published on.
        assert!(p
            .check(blog, Some("wan"), "blog.home.example.org")
            .is_some());
        // And it is the caller's own routes, not anyone's.
        assert!(p
            .check(peer("10.244.2.5"), Some("wan"), "blog.home.example.org")
            .is_some());
        // Other hosts stay denied.
        assert!(p
            .check(blog, Some("tailscale"), "evil.home.example.org")
            .is_some());
    }

    #[test]
    fn an_empty_list_permits_nothing() {
        assert!(!HostAllow::new(Vec::<String>::new()).permits("anything.example.org"));
    }

    #[test]
    fn listed_hosts_pass_and_others_are_denied_with_the_reason() {
        let p = policies();
        assert_eq!(
            p.check(peer("10.244.1.5"), Some("wan"), "auth.example.org"),
            None
        );
        let d = p
            .check(peer("10.244.1.5"), Some("wan"), "other.example.org:443")
            .unwrap();
        assert_eq!(
            d,
            Denied {
                stack: "blog".into(),
                network: "wan".into(),
                host: "other.example.org".into()
            }
        );
    }

    #[test]
    fn each_network_has_its_own_list() {
        let p = policies();
        // Listed on tailscale, not on wan.
        assert!(p
            .check(peer("10.244.1.5"), Some("wan"), "cloud.home.example.org")
            .is_some());
        assert!(p
            .check(
                peer("10.244.1.5"),
                Some("tailscale"),
                "cloud.home.example.org"
            )
            .is_none());
        assert!(p
            .check(peer("10.244.1.5"), Some("tailscale"), "a.apps.example.org")
            .is_none());
    }

    #[test]
    fn a_network_without_a_list_is_unrestricted() {
        let p = policies();
        assert!(p
            .check(peer("10.244.1.5"), Some("lan"), "anything")
            .is_none());
        assert!(p.check(peer("10.244.1.5"), None, "anything").is_none());
    }

    #[test]
    fn an_empty_list_denies_every_host_on_its_network() {
        let p = policies();
        assert!(p
            .check(peer("10.244.1.5"), Some("sealed"), "a.example.org")
            .is_some());
    }

    #[test]
    fn peers_outside_every_restricted_subnet_are_untouched() {
        let p = policies();
        assert!(p
            .check(peer("10.244.3.9"), Some("wan"), "evil.example.org")
            .is_none());
        assert!(p
            .check(peer("192.168.1.2"), Some("wan"), "evil.example.org")
            .is_none());
        assert!(p
            .check(peer("::1"), Some("wan"), "evil.example.org")
            .is_none());
    }

    #[test]
    fn stacks_do_not_share_lists() {
        let p = policies();
        assert!(p
            .check(peer("10.244.2.5"), Some("wan"), "auth.example.org")
            .is_some());
        assert!(p
            .check(peer("10.244.2.5"), Some("wan"), "x.example.org")
            .is_none());
    }

    #[test]
    fn a_v4_mapped_peer_is_matched_as_v4() {
        let p = policies();
        assert!(p
            .check(peer("::ffff:10.244.1.5"), Some("wan"), "evil.example.org")
            .is_some());
    }

    #[test]
    fn an_empty_table_is_a_no_op() {
        let p = PeerPolicies::new();
        assert!(p.is_empty());
        assert!(p.check(peer("10.244.1.5"), Some("wan"), "x").is_none());
        let p = policies();
        p.replace(Vec::new());
        assert!(p.is_empty());
    }
}
