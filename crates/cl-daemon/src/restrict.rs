//! Which stacks declared an `allowed` list, and what that means for the proxy
//! and the host firewall.
//!
//! Everything here is derived from replicated state, so every node reaches the
//! same answer: a stack's overlay subnet comes from the IPAM allocations, and
//! the lists from its stored stack file. The proxy side (hostname entries) is
//! [`proxy_policies`], the firewall side (`host:port` entries, resolved to
//! addresses) is [`restrictions`]. Name resolution is the only impure part and
//! lives in [`HostResolver`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::Ipv4Addr;
use std::time::Duration;

use cl_config::{qualify, AllowedHost, AllowedTarget, ClusterConfig, StackFile};
use cl_net::restrict::{PortGrant, Restriction};
use cl_net::Allocations;
use cl_proxy::{HostAllow, StackPolicy};
use cl_raft::state::ClusterState;
use cl_sched::run::overlay_name;
use ipnet::Ipv4Net;
use tracing::debug;

/// How long one name gets to resolve. A tick must not stall on a dead
/// resolver, and the previous answer is kept when it times out.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);

/// A stack with at least one `allowed` list and an overlay subnet to identify
/// its containers by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestrictedStack {
    pub stack: String,
    pub subnet: Ipv4Net,
    /// Every network that declared a list, by name.
    pub networks: BTreeMap<String, Vec<AllowedTarget>>,
    /// Fully qualified hostnames the stack's own services publish through the
    /// proxy, by network. The proxy lets the stack reach these regardless of
    /// its lists.
    pub own_routes: BTreeMap<String, Vec<String>>,
    /// The network the containers live on. The firewall restricts the stack
    /// only when that network declared a list.
    pub overlay_network: String,
}

impl RestrictedStack {
    fn firewalled(&self) -> Option<&Vec<AllowedTarget>> {
        self.networks.get(&self.overlay_network)
    }
}

/// Every stack that declared an `allowed` list, in stack name order. A stack
/// whose overlay has no subnet yet is skipped: it has no containers either.
///
/// `None` means the answer cannot be trusted: no cluster config replicated yet
/// (a node that just started, before state arrives), a stack file that does not
/// parse, or lists declared but no readable allocations. Callers must keep what
/// they last applied in that case; treating it as "nothing is restricted"
/// would drop the firewall under running containers.
pub fn restricted_stacks(
    cfg: &ClusterConfig,
    state: &ClusterState,
) -> Option<Vec<RestrictedStack>> {
    state.cluster_config.as_ref()?;
    let mut declared = Vec::new();
    for (name, stack) in &state.stacks {
        let file = StackFile::parse(&stack.spec).ok()?;
        let networks: BTreeMap<String, Vec<AllowedTarget>> = file
            .networks
            .iter()
            .filter_map(|(net, def)| Some((net.clone(), def.as_ref()?.allowed.clone()?)))
            .collect();
        if !networks.is_empty() {
            declared.push((name, file, networks));
        }
    }
    if declared.is_empty() {
        return Some(Vec::new());
    }
    let allocations = Allocations::decode(state.overlay.as_deref())?;
    let mut out = Vec::new();
    for (name, file, networks) in declared {
        let Some(overlay) = file.overlay_network(cfg) else {
            continue;
        };
        let Some(subnet) = allocations.subnet(&overlay_name(name, overlay)) else {
            continue;
        };
        let mut own_routes: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for entry in file
            .services
            .values()
            .filter_map(|svc| svc.x_clouduler.as_ref())
            .flat_map(|ext| &ext.proxy)
        {
            let domain = cfg
                .networks
                .get(&entry.network)
                .and_then(|n| n.domain.as_deref());
            own_routes
                .entry(entry.network.clone())
                .or_default()
                .push(qualify(&entry.host, domain));
        }
        out.push(RestrictedStack {
            stack: name.clone(),
            subnet,
            networks,
            own_routes,
            overlay_network: overlay.to_string(),
        });
    }
    Some(out)
}

fn host_string(host: &AllowedHost) -> String {
    match host {
        AllowedHost::Name(n) => n.clone(),
        AllowedHost::Wildcard(s) => format!("*.{s}"),
        AllowedHost::Ip(ip) => ip.to_string(),
    }
}

/// The proxy's view: per overlay subnet, the hostname-only entries of every
/// network the stack listed.
pub fn proxy_policies(stacks: &[RestrictedStack]) -> Vec<(Ipv4Net, StackPolicy)> {
    stacks
        .iter()
        .map(|s| {
            let networks: HashMap<String, HostAllow> = s
                .networks
                .iter()
                .map(|(net, entries)| {
                    let hosts = entries
                        .iter()
                        .filter(|e| e.ports.is_none())
                        .map(|e| host_string(&e.host));
                    (net.clone(), HostAllow::new(hosts))
                })
                .collect();
            let own_routes = s
                .own_routes
                .iter()
                .map(|(net, hosts)| (net.clone(), HostAllow::new(hosts)))
                .collect();
            (
                s.subnet,
                StackPolicy {
                    stack: s.stack.clone(),
                    networks,
                    own_routes,
                },
            )
        })
        .collect()
}

/// Names the firewall needs resolved.
pub fn names_to_resolve(stacks: &[RestrictedStack]) -> BTreeSet<String> {
    stacks
        .iter()
        .filter_map(RestrictedStack::firewalled)
        .flatten()
        .filter(|e| e.ports.is_some())
        .filter_map(|e| match &e.host {
            AllowedHost::Name(n) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

/// The firewall's view: one [`Restriction`] per stack whose overlay network
/// declared a list. A name with no known address contributes no grant, so an
/// unresolved destination stays closed.
pub fn restrictions(
    stacks: &[RestrictedStack],
    resolved: &BTreeMap<String, Vec<Ipv4Addr>>,
) -> Vec<Restriction> {
    stacks
        .iter()
        .filter_map(|s| {
            let entries = s.firewalled()?;
            let mut grants = Vec::new();
            for entry in entries {
                let Some(ports) = entry.ports else { continue };
                let addrs: Vec<Ipv4Addr> = match &entry.host {
                    AllowedHost::Ip(ip) => vec![*ip],
                    AllowedHost::Name(n) => resolved.get(n).cloned().unwrap_or_default(),
                    AllowedHost::Wildcard(_) => Vec::new(),
                };
                grants.extend(addrs.into_iter().map(|addr| PortGrant {
                    protocol: ports.protocol,
                    addr,
                    ports: ports.range,
                }));
            }
            Some(Restriction {
                subnet: s.subnet,
                stack_subnets: vec![s.subnet],
                grants,
            })
        })
        .collect()
}

/// Resolves the hostnames of `host:port` entries, remembering the last good
/// answer so a flaky resolver does not slam the door on a stack every time.
#[derive(Debug, Default)]
pub struct HostResolver {
    cache: BTreeMap<String, Vec<Ipv4Addr>>,
}

impl HostResolver {
    pub fn resolved(&self) -> &BTreeMap<String, Vec<Ipv4Addr>> {
        &self.cache
    }

    /// Re-resolve `names`. A failed or timed out lookup keeps what was known;
    /// a name nobody asks for any more is forgotten.
    pub async fn refresh(&mut self, names: &BTreeSet<String>) {
        self.cache.retain(|n, _| names.contains(n));
        let lookups = names.iter().map(|name| async move {
            let query = format!("{name}:0");
            let answer =
                tokio::time::timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host(query)).await;
            (name.clone(), answer)
        });
        for (name, answer) in futures::future::join_all(lookups).await {
            match answer {
                Ok(Ok(addrs)) => {
                    let mut v4: Vec<Ipv4Addr> = addrs
                        .filter_map(|a| match a.ip() {
                            std::net::IpAddr::V4(ip) => Some(ip),
                            _ => None,
                        })
                        .collect();
                    v4.sort();
                    v4.dedup();
                    self.cache.insert(name, v4);
                }
                Ok(Err(e)) => {
                    debug!(%name, error = %e, "allowed host did not resolve, keeping the last answer")
                }
                Err(_) => debug!(%name, "allowed host lookup timed out, keeping the last answer"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_config::{PortRange, Protocol};
    use cl_net::Allocations;

    fn targets(entries: &[&str]) -> Vec<AllowedTarget> {
        entries
            .iter()
            .map(|e| AllowedTarget::parse(e).unwrap())
            .collect()
    }

    fn stack(overlay: &str, lists: &[(&str, &[&str])]) -> RestrictedStack {
        RestrictedStack {
            stack: "blog".into(),
            subnet: "10.244.1.0/24".parse().unwrap(),
            networks: lists
                .iter()
                .map(|(n, e)| (n.to_string(), targets(e)))
                .collect(),
            own_routes: BTreeMap::new(),
            overlay_network: overlay.into(),
        }
    }

    const CLUSTER: &str = include_str!("../../../examples/cluster.yaml");

    fn cluster() -> ClusterConfig {
        ClusterConfig::parse(CLUSTER).unwrap()
    }

    fn state_with(stacks: &[(&str, &str)], allocate: &[&str], config: bool) -> ClusterState {
        let mut state = ClusterState::default();
        if config {
            state.cluster_config = Some(cl_raft::state::ClusterConfigEntry {
                yaml: CLUSTER.into(),
                generation: 1,
            });
        }
        for (name, spec) in stacks {
            state.stacks.insert(
                name.to_string(),
                serde_json::from_value(serde_json::json!({
                    "name": name, "spec": spec, "revision": 1
                }))
                .unwrap(),
            );
        }
        if !allocate.is_empty() {
            let mut alloc = Allocations::new("10.244.0.0/16".parse().unwrap(), 24);
            for n in allocate {
                alloc.allocate_network(n, &[]).unwrap();
            }
            state.overlay = Some(serde_json::to_string(&alloc).unwrap());
        }
        state
    }

    const RESTRICTED: &str =
        "services:\n  web:\n    image: x\nnetworks:\n  tailscale:\n    allowed: [a.example.org]\n";
    const PLAIN: &str = "services:\n  web:\n    image: x\nnetworks:\n  tailscale: {}\n";

    #[test]
    fn restricted_stacks_finds_the_stack_and_its_subnet() {
        let state = state_with(
            &[("blog", RESTRICTED), ("wiki", PLAIN)],
            &["blog_tailscale", "wiki_tailscale"],
            true,
        );
        let found = restricted_stacks(&cluster(), &state).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].stack, "blog");
        assert_eq!(found[0].overlay_network, "tailscale");
        assert_eq!(found[0].subnet.prefix_len(), 24);
    }

    #[test]
    fn a_stack_without_an_allocation_yet_is_skipped_not_fatal() {
        let state = state_with(
            &[("blog", RESTRICTED), ("x", PLAIN)],
            &["x_tailscale"],
            true,
        );
        assert_eq!(restricted_stacks(&cluster(), &state), Some(vec![]));
    }

    #[test]
    fn nothing_declared_is_a_known_empty_answer_even_without_allocations() {
        let state = state_with(&[("wiki", PLAIN)], &[], true);
        assert_eq!(restricted_stacks(&cluster(), &state), Some(vec![]));
    }

    #[test]
    fn unknown_state_is_never_read_as_nothing_restricted() {
        // No cluster config replicated yet (a node that just booted).
        let booting = state_with(&[("blog", RESTRICTED)], &["blog_tailscale"], false);
        assert_eq!(restricted_stacks(&cluster(), &booting), None);
        // Lists declared but the address book is unreadable.
        let no_book = state_with(&[("blog", RESTRICTED)], &[], true);
        assert_eq!(restricted_stacks(&cluster(), &no_book), None);
        // A stored stack that no longer parses.
        let broken = state_with(
            &[("blog", "services: [not a map"), ("ok", RESTRICTED)],
            &["ok_tailscale"],
            true,
        );
        assert_eq!(restricted_stacks(&cluster(), &broken), None);
    }

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn the_proxy_gets_hostname_entries_per_network_and_never_port_entries() {
        let s = stack(
            "tailscale",
            &[
                (
                    "tailscale",
                    &[
                        "cloud.example.org",
                        "svc.example.org:22",
                        "*.apps.example.org",
                    ],
                ),
                ("wan", &["auth.example.org"]),
            ],
        );
        let policies = proxy_policies(&[s]);
        assert_eq!(policies.len(), 1);
        let (subnet, policy) = &policies[0];
        assert_eq!(subnet.to_string(), "10.244.1.0/24");
        assert_eq!(policy.stack, "blog");
        let ts = &policy.networks["tailscale"];
        assert!(ts.permits("cloud.example.org"));
        assert!(ts.permits("x.apps.example.org"));
        assert!(
            !ts.permits("svc.example.org"),
            "a port entry is not a proxy grant"
        );
        assert!(policy.networks["wan"].permits("auth.example.org"));
        assert!(!policy.networks["wan"].permits("cloud.example.org"));
    }

    #[test]
    fn an_empty_list_is_a_policy_that_permits_nothing() {
        let s = stack("tailscale", &[("tailscale", &[])]);
        let policies = proxy_policies(&[s]);
        assert!(!policies[0].1.networks["tailscale"].permits("anything.example.org"));
    }

    #[test]
    fn the_firewall_restricts_only_when_the_overlay_network_declared_a_list() {
        let only_wan = stack("tailscale", &[("wan", &["auth.example.org"])]);
        assert!(restrictions(&[only_wan], &BTreeMap::new()).is_empty());
        let on_overlay = stack("tailscale", &[("tailscale", &["auth.example.org"])]);
        let r = restrictions(&[on_overlay], &BTreeMap::new());
        assert_eq!(r.len(), 1);
        assert!(
            r[0].grants.is_empty(),
            "hostname-only entries grant nothing"
        );
    }

    #[test]
    fn port_entries_become_grants_on_every_resolved_address() {
        let s = stack(
            "tailscale",
            &[(
                "tailscale",
                &[
                    "svc.example.org:22",
                    "10.9.9.9:8000-8100/udp",
                    "gone.example.org:1",
                ],
            )],
        );
        let resolved = BTreeMap::from([(
            "svc.example.org".to_string(),
            vec![ip("100.64.0.5"), ip("100.64.0.6")],
        )]);
        let r = restrictions(&[s], &resolved);
        let mut grants = r[0].grants.clone();
        grants.sort();
        assert_eq!(
            grants,
            vec![
                PortGrant {
                    protocol: Protocol::Tcp,
                    addr: ip("100.64.0.5"),
                    ports: PortRange { start: 22, end: 22 }
                },
                PortGrant {
                    protocol: Protocol::Tcp,
                    addr: ip("100.64.0.6"),
                    ports: PortRange { start: 22, end: 22 }
                },
                PortGrant {
                    protocol: Protocol::Udp,
                    addr: ip("10.9.9.9"),
                    ports: PortRange {
                        start: 8000,
                        end: 8100
                    }
                },
            ]
        );
    }

    #[test]
    fn only_firewalled_port_entry_names_are_resolved() {
        let s = stack(
            "tailscale",
            &[
                (
                    "tailscale",
                    &[
                        "a.example.org:22",
                        "b.example.org",
                        "10.0.0.1:22",
                        "*.c.example.org",
                    ],
                ),
                ("wan", &["d.example.org"]),
            ],
        );
        assert_eq!(
            names_to_resolve(&[s]),
            BTreeSet::from(["a.example.org".to_string()])
        );
    }

    #[tokio::test]
    async fn the_resolver_resolves_literals_and_forgets_unwanted_names() {
        let mut r = HostResolver::default();
        r.refresh(&BTreeSet::from([
            "127.0.0.1".to_string(),
            "localhost".to_string(),
        ]))
        .await;
        assert_eq!(r.resolved()["127.0.0.1"], vec![ip("127.0.0.1")]);
        assert!(r.resolved()["localhost"].contains(&ip("127.0.0.1")));
        r.refresh(&BTreeSet::from(["127.0.0.1".to_string()])).await;
        assert!(!r.resolved().contains_key("localhost"));
    }

    #[tokio::test]
    async fn a_failed_lookup_keeps_the_previous_answer() {
        let mut r = HostResolver::default();
        r.cache.insert("nope.invalid".into(), vec![ip("1.2.3.4")]);
        r.refresh(&BTreeSet::from(["nope.invalid".to_string()]))
            .await;
        assert_eq!(r.resolved()["nope.invalid"], vec![ip("1.2.3.4")]);
    }
}
