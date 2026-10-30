//! `allowed` lists on wan (egress, wireguard and nat) networks.
//!
//! The bridge of such a network is shared by every stack on it, so a stack is
//! recognised by the addresses its containers hold there ([`start`] reads them
//! from docker every second, a new container being open to the world for as
//! long as that takes). What a stack may reach is its list: IPv4 literals
//! straight away, and hostnames (or `*.suffix` patterns) on whatever addresses
//! the stack's own DNS lookups of them return. The cluster resolver is what
//! sees those lookups: [`WanFirewall`] is its [`AnswerObserver`], and it only
//! lets the answer back to the container once the firewall carries the grant,
//! so the connection that follows is never refused for being early.
//!
//! The firewall rules themselves are written by the egress reconcile loop,
//! into the same chain as the overlay restrictions (see
//! `cl_net::restrict::forward_plan_with_wan`): this module only holds the
//! state and wakes that loop when it changes.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cl_config::{AllowedHost, AllowedTarget, ClusterConfig, NetworkKind};
use cl_docker::{Docker, LABEL_STACK};
use cl_net::dns::AnswerObserver;
use cl_net::egress::{egress_docker_name, nat_docker_name};
use cl_net::restrict::{WanGrant, WanRestriction};
use cl_raft::state::ClusterState;
use cl_raft::ClusterNode;
use ipnet::Ipv4Net;
use tokio::sync::watch;
use tracing::{debug, warn};

use crate::restrict::{restricted_stacks, RestrictedStack};

/// How often container addresses are re-read. This is the longest a new
/// container of a restricted stack can reach anything it likes.
const SOURCE_INTERVAL: Duration = Duration::from_secs(1);

/// A name's grant lasts at least this long, however short the record's TTL:
/// applications keep connecting to addresses they resolved a while ago, and
/// a lookup renews the grant anyway.
const MIN_GRANT: Duration = Duration::from_secs(300);

/// The longest a lookup waits for the firewall to carry its grant before the
/// answer goes back regardless.
const APPLY_WAIT: Duration = Duration::from_secs(2);

/// A stack with an `allowed` list on at least one wan network, and the overlay
/// subnet its containers resolve names from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WanStack {
    pub stack: String,
    pub subnet: Ipv4Net,
    /// Wan network name to its list.
    pub networks: BTreeMap<String, Vec<AllowedTarget>>,
}

/// The wan lists of the restricted stacks, from their declared lists and the
/// cluster's network kinds.
pub fn wan_stacks(cfg: &ClusterConfig, stacks: &[RestrictedStack]) -> Vec<WanStack> {
    let wan = cfg.egress_networks();
    stacks
        .iter()
        .filter_map(|s| {
            let networks: BTreeMap<String, Vec<AllowedTarget>> = s
                .networks
                .iter()
                .filter(|(name, _)| wan.contains(*name))
                .map(|(name, list)| (name.clone(), list.clone()))
                .collect();
            (!networks.is_empty()).then(|| WanStack {
                stack: s.stack.clone(),
                subnet: s.subnet,
                networks,
            })
        })
        .collect()
}

/// Does `name` (lowercase, no trailing dot) fall under the entry's host? A
/// wildcard matches any number of labels below its suffix, never the suffix
/// itself: a lookup of `a.b.example.org` is as much `*.example.org` as
/// `b.example.org` is, since the answer is often a chain of names under it.
fn host_matches(host: &AllowedHost, name: &str) -> bool {
    match host {
        AllowedHost::Name(n) => n == name,
        AllowedHost::Wildcard(suffix) => name
            .strip_suffix(suffix.as_str())
            .is_some_and(|rest| rest.len() > 1 && rest.ends_with('.')),
        AllowedHost::Ip(_) => false,
    }
}

/// The grants an IPv4 literal entry gives, which hold from the start.
pub fn static_grants(entries: &[AllowedTarget]) -> Vec<WanGrant> {
    entries
        .iter()
        .filter_map(|e| match e.host {
            AllowedHost::Ip(addr) => Some(WanGrant {
                addr,
                ports: e.ports.map(|p| (p.protocol, p.range)),
            }),
            _ => None,
        })
        .collect()
}

/// The grants the answer to a lookup of `name` gives: every address, once per
/// entry that names it.
pub fn answer_grants(
    entries: &[AllowedTarget],
    name: &str,
    answers: &[(Ipv4Addr, u32)],
) -> Vec<(WanGrant, u32)> {
    let mut out = Vec::new();
    for entry in entries.iter().filter(|e| host_matches(&e.host, name)) {
        for (addr, ttl) in answers {
            out.push((
                WanGrant {
                    addr: *addr,
                    ports: entry.ports.map(|p| (p.protocol, p.range)),
                },
                *ttl,
            ));
        }
    }
    out
}

type Key = (String, String);

#[derive(Default)]
struct Inner {
    stacks: Vec<WanStack>,
    /// The addresses of each (stack, network)'s containers on the bridge.
    sources: BTreeMap<Key, Vec<Ipv4Addr>>,
    /// Grants made by lookups, with the instant each lapses.
    dynamic: BTreeMap<Key, BTreeMap<WanGrant, Instant>>,
}

/// Shared between the resolver (which grants), the container reader (which
/// supplies the sources) and the egress reconcile loop (which writes rules).
pub struct WanFirewall {
    inner: Mutex<Inner>,
    /// Bumped whenever what the rules should say changes.
    requested: watch::Sender<u64>,
    /// The `requested` value the rules last written were computed at.
    applied: watch::Sender<u64>,
}

impl WanFirewall {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::default(),
            requested: watch::channel(0).0,
            applied: watch::channel(0).0,
        })
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bump(&self) {
        self.requested.send_modify(|v| *v += 1);
    }

    /// Wakes when the rules need rewriting.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.requested.subscribe()
    }

    /// The version the next [`restrictions`](Self::restrictions) call reflects
    /// at least. Read it before, then report it with [`mark_applied`](Self::mark_applied).
    pub fn version(&self) -> u64 {
        *self.requested.borrow()
    }

    /// The rules for `version` are in place.
    pub fn mark_applied(&self, version: u64) {
        self.applied.send_if_modified(|v| {
            let newer = version > *v;
            if newer {
                *v = version;
            }
            newer
        });
    }

    /// The stacks with a wan list. Grants of a stack or network that no longer
    /// has one are forgotten.
    pub fn set_stacks(&self, stacks: Vec<WanStack>) {
        let mut inner = self.inner();
        if inner.stacks == stacks {
            return;
        }
        let kept: BTreeSet<Key> = stacks
            .iter()
            .flat_map(|s| s.networks.keys().map(|n| (s.stack.clone(), n.clone())))
            .collect();
        inner.dynamic.retain(|k, _| kept.contains(k));
        inner.sources.retain(|k, _| kept.contains(k));
        inner.stacks = stacks;
        drop(inner);
        self.bump();
    }

    /// The (stack, network) pairs that need their containers' addresses.
    pub fn networks(&self) -> BTreeSet<Key> {
        self.inner()
            .stacks
            .iter()
            .flat_map(|s| s.networks.keys().map(|n| (s.stack.clone(), n.clone())))
            .collect()
    }

    pub fn set_sources(&self, sources: BTreeMap<Key, Vec<Ipv4Addr>>) {
        let mut inner = self.inner();
        if inner.sources == sources {
            return;
        }
        inner.sources = sources;
        drop(inner);
        self.bump();
    }

    /// Drop grants that have lapsed.
    pub fn prune(&self, now: Instant) {
        let mut changed = false;
        for grants in self.inner().dynamic.values_mut() {
            let before = grants.len();
            grants.retain(|_, until| *until > now);
            changed |= grants.len() != before;
        }
        if changed {
            self.bump();
        }
    }

    /// One restriction per (stack, network) with containers on the bridge.
    /// One with none restricts nothing, there being nobody to drop.
    pub fn restrictions(&self, now: Instant) -> Vec<WanRestriction> {
        let inner = self.inner();
        let mut out = Vec::new();
        for stack in &inner.stacks {
            for (network, entries) in &stack.networks {
                let key = (stack.stack.clone(), network.clone());
                let Some(sources) = inner.sources.get(&key).filter(|s| !s.is_empty()) else {
                    continue;
                };
                let mut grants: BTreeSet<WanGrant> = static_grants(entries).into_iter().collect();
                if let Some(dynamic) = inner.dynamic.get(&key) {
                    grants.extend(
                        dynamic
                            .iter()
                            .filter(|(_, until)| **until > now)
                            .map(|(g, _)| *g),
                    );
                }
                out.push(WanRestriction {
                    sources: sources.clone(),
                    grants: grants.into_iter().collect(),
                });
            }
        }
        out
    }

    /// Record what `client`'s lookup of `name` returned. Returns the version
    /// the rules must reach before the answer is let go, if anything changed.
    fn grant(
        &self,
        now: Instant,
        client: Ipv4Addr,
        name: &str,
        answers: &[(Ipv4Addr, u32)],
    ) -> Option<u64> {
        let mut inner = self.inner();
        let Inner {
            stacks, dynamic, ..
        } = &mut *inner;
        let stack = stacks.iter().find(|s| s.subnet.contains(&client))?;
        let mut added = false;
        for (network, entries) in &stack.networks {
            let slot = dynamic
                .entry((stack.stack.clone(), network.clone()))
                .or_default();
            for (grant, ttl) in answer_grants(entries, name, answers) {
                let until = now + Duration::from_secs(u64::from(ttl)).max(MIN_GRANT);
                match slot.insert(grant, until) {
                    Some(previous) if previous > now => {}
                    _ => added = true,
                }
            }
        }
        drop(inner);
        added.then(|| {
            self.bump();
            self.version()
        })
    }
}

#[async_trait::async_trait]
impl AnswerObserver for WanFirewall {
    async fn observe(&self, client: IpAddr, name: &str, answers: &[(Ipv4Addr, u32)]) {
        let IpAddr::V4(client) = client else { return };
        let Some(version) = self.grant(Instant::now(), client, name, answers) else {
            return;
        };
        let mut applied = self.applied.subscribe();
        if tokio::time::timeout(APPLY_WAIT, applied.wait_for(|v| *v >= version))
            .await
            .is_err()
        {
            warn!(%name, "the firewall did not take a wan grant in time, answering anyway");
        }
    }
}

/// The docker network a wan network's bridge is.
fn docker_network(cfg: &ClusterConfig, network: &str) -> String {
    match cfg.networks.get(network).map(|n| n.kind) {
        Some(NetworkKind::Nat) => nat_docker_name(network),
        _ => egress_docker_name(network),
    }
}

/// What the stacks and the cluster config looked like when the lists were last
/// read, so they are only parsed again when one of them changed.
#[derive(Default, PartialEq, Eq)]
struct Signature {
    revisions: BTreeMap<String, u64>,
    config: Option<u64>,
    overlay: u64,
}

impl Signature {
    fn of(state: &ClusterState) -> Self {
        let mut hasher = DefaultHasher::new();
        state.overlay.hash(&mut hasher);
        Self {
            revisions: state
                .stacks
                .iter()
                .map(|(n, s)| (n.clone(), s.revision))
                .collect(),
            config: state.cluster_config.as_ref().map(|c| c.generation),
            overlay: hasher.finish(),
        }
    }
}

/// Keep `wan` in step with the stacks' lists and the containers on the bridges.
pub fn start(
    wan: Arc<WanFirewall>,
    cluster: Arc<ClusterNode>,
    docker: Arc<tokio::sync::RwLock<Option<Docker>>>,
    boot_cfg: ClusterConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SOURCE_INTERVAL);
        let mut seen = Signature::default();
        loop {
            ticker.tick().await;
            let state = cluster.state().await;
            let cfg = crate::daemon::live_config(&state, &boot_cfg);
            let signature = Signature::of(&state);
            if signature != seen {
                // Unknown state keeps what was read before, see
                // `restricted_stacks`.
                if let Some(stacks) = restricted_stacks(&cfg, &state) {
                    wan.set_stacks(wan_stacks(&cfg, &stacks));
                    seen = signature;
                }
            }
            wan.prune(Instant::now());

            let wanted = wan.networks();
            if wanted.is_empty() {
                continue;
            }
            let Some(docker) = docker.read().await.clone() else {
                continue;
            };
            match read_sources(&docker, &cfg, &wanted).await {
                Ok(sources) => wan.set_sources(sources),
                Err(e) => {
                    debug!(error = %e, "cannot read the containers on the wan bridges, keeping the last answer")
                }
            }
        }
    })
}

/// The addresses on the bridge of every (stack, network) in `wanted`.
async fn read_sources(
    docker: &Docker,
    cfg: &ClusterConfig,
    wanted: &BTreeSet<Key>,
) -> cl_docker::Result<BTreeMap<Key, Vec<Ipv4Addr>>> {
    let owners: BTreeMap<String, String> = docker
        .list_managed()
        .await?
        .into_iter()
        .filter_map(|c| Some((c.id, c.labels.get(LABEL_STACK)?.clone())))
        .collect();
    let mut sources: BTreeMap<Key, Vec<Ipv4Addr>> = BTreeMap::new();
    let networks: BTreeSet<&String> = wanted.iter().map(|(_, n)| n).collect();
    for network in networks {
        for (id, addr) in docker
            .network_members(&docker_network(cfg, network))
            .await?
        {
            let Some(stack) = owners.get(&id) else {
                continue;
            };
            let key = (stack.clone(), network.clone());
            if wanted.contains(&key) {
                sources.entry(key).or_default().push(addr);
            }
        }
    }
    for addrs in sources.values_mut() {
        addrs.sort();
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_config::{PortRange, Protocol};

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    fn entries(list: &[&str]) -> Vec<AllowedTarget> {
        list.iter()
            .map(|e| AllowedTarget::parse(e).unwrap())
            .collect()
    }

    fn firewall(list: &[&str]) -> Arc<WanFirewall> {
        let wan = WanFirewall::new();
        wan.set_stacks(vec![WanStack {
            stack: "blog".into(),
            subnet: "10.244.1.0/24".parse().unwrap(),
            networks: BTreeMap::from([("vpn".to_string(), entries(list))]),
        }]);
        wan.set_sources(BTreeMap::from([(
            ("blog".to_string(), "vpn".to_string()),
            vec![ip("10.245.3.2")],
        )]));
        wan
    }

    fn anywhere(addr: &str) -> WanGrant {
        WanGrant {
            addr: ip(addr),
            ports: None,
        }
    }

    #[test]
    fn a_name_entry_matches_only_that_name() {
        let list = entries(&["api.example.org"]);
        assert_eq!(
            answer_grants(&list, "api.example.org", &[(ip("1.1.1.1"), 60)]).len(),
            1
        );
        assert!(answer_grants(&list, "www.example.org", &[(ip("1.1.1.1"), 60)]).is_empty());
        assert!(answer_grants(&list, "x.api.example.org", &[(ip("1.1.1.1"), 60)]).is_empty());
    }

    #[test]
    fn a_wildcard_matches_any_depth_below_but_not_the_apex() {
        let list = entries(&["*.example.org"]);
        for name in ["a.example.org", "a.b.example.org"] {
            assert_eq!(
                answer_grants(&list, name, &[(ip("1.1.1.1"), 60)]).len(),
                1,
                "{name}"
            );
        }
        for name in ["example.org", "notexample.org", "example.org.evil.com"] {
            assert!(
                answer_grants(&list, name, &[(ip("1.1.1.1"), 60)]).is_empty(),
                "{name}"
            );
        }
    }

    #[test]
    fn a_port_on_the_entry_limits_the_grant() {
        let list = entries(&["*.example.org:443", "api.example.org:8000-8100/udp"]);
        let grants = answer_grants(&list, "api.example.org", &[(ip("1.1.1.1"), 60)]);
        let ports: Vec<_> = grants.iter().map(|(g, _)| g.ports.unwrap()).collect();
        assert!(ports.contains(&(
            Protocol::Tcp,
            PortRange {
                start: 443,
                end: 443
            }
        )));
        assert!(ports.contains(&(
            Protocol::Udp,
            PortRange {
                start: 8000,
                end: 8100
            }
        )));
    }

    #[test]
    fn ip_literals_are_granted_from_the_start() {
        let wan = firewall(&["9.9.9.9", "8.8.8.8:53/udp"]);
        let r = wan.restrictions(Instant::now());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].sources, vec![ip("10.245.3.2")]);
        assert!(r[0].grants.contains(&anywhere("9.9.9.9")));
        assert!(r[0]
            .grants
            .iter()
            .any(|g| g.addr == ip("8.8.8.8") && g.ports.is_some()));
    }

    #[test]
    fn a_lookup_grants_until_the_ttl_or_the_minimum_whichever_is_longer() {
        let wan = firewall(&["api.example.org"]);
        let now = Instant::now();
        let before = wan.version();
        let v = wan.grant(
            now,
            ip("10.244.1.5"),
            "api.example.org",
            &[(ip("1.1.1.1"), 10)],
        );
        assert_eq!(v, Some(before + 1));
        assert!(wan.restrictions(now)[0]
            .grants
            .contains(&anywhere("1.1.1.1")));
        let later = now + MIN_GRANT - Duration::from_secs(1);
        assert!(wan.restrictions(later)[0]
            .grants
            .contains(&anywhere("1.1.1.1")));
        let lapsed = now + MIN_GRANT + Duration::from_secs(1);
        assert!(!wan.restrictions(lapsed)[0]
            .grants
            .contains(&anywhere("1.1.1.1")));
    }

    #[test]
    fn asking_again_for_a_live_grant_does_not_rewrite_the_rules() {
        let wan = firewall(&["api.example.org"]);
        let now = Instant::now();
        let answers = [(ip("1.1.1.1"), 60)];
        assert!(wan
            .grant(now, ip("10.244.1.5"), "api.example.org", &answers)
            .is_some());
        assert!(wan
            .grant(now, ip("10.244.1.5"), "api.example.org", &answers)
            .is_none());
    }

    #[test]
    fn a_name_not_on_the_list_or_a_stranger_grants_nothing() {
        let wan = firewall(&["api.example.org"]);
        let now = Instant::now();
        let answers = [(ip("1.1.1.1"), 60)];
        assert!(wan
            .grant(now, ip("10.244.1.5"), "evil.example.org", &answers)
            .is_none());
        assert!(wan
            .grant(now, ip("10.244.7.5"), "api.example.org", &answers)
            .is_none());
        assert!(wan.restrictions(now)[0].grants.is_empty());
    }

    #[test]
    fn an_empty_list_is_a_restriction_with_no_grants() {
        let wan = firewall(&[]);
        let r = wan.restrictions(Instant::now());
        assert_eq!(r.len(), 1);
        assert!(r[0].grants.is_empty());
    }

    #[test]
    fn nothing_is_restricted_until_a_container_is_on_the_bridge() {
        let wan = firewall(&["api.example.org"]);
        wan.set_sources(BTreeMap::new());
        assert!(wan.restrictions(Instant::now()).is_empty());
    }

    #[test]
    fn dropping_the_list_forgets_its_grants() {
        let wan = firewall(&["api.example.org"]);
        let now = Instant::now();
        wan.grant(
            now,
            ip("10.244.1.5"),
            "api.example.org",
            &[(ip("1.1.1.1"), 60)],
        );
        wan.set_stacks(Vec::new());
        assert!(wan.restrictions(now).is_empty());
        assert!(wan.inner().dynamic.is_empty());
    }

    #[tokio::test]
    async fn a_lookup_waits_for_the_rules_that_carry_its_grant() {
        let wan = firewall(&["api.example.org"]);
        let mut wake = wan.subscribe();
        let observer = wan.clone();
        let lookup = tokio::spawn(async move {
            observer
                .observe(
                    IpAddr::V4(ip("10.244.1.5")),
                    "api.example.org",
                    &[(ip("1.1.1.1"), 60)],
                )
                .await;
        });
        wake.changed().await.unwrap();
        tokio::task::yield_now().await;
        assert!(
            !lookup.is_finished(),
            "answered before the rules were written"
        );
        let version = wan.version();
        assert!(wan
            .restrictions(Instant::now())
            .iter()
            .any(|r| r.grants.contains(&anywhere("1.1.1.1"))));
        wan.mark_applied(version);
        lookup.await.unwrap();
    }
}
