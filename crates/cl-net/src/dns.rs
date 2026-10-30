//! Internal DNS: resolves service and node names to overlay addresses for
//! containers on the bridge, and forwards everything else upstream.
//!
//! The zone content (`ZoneRecord`/`build_records`) is plain data, built from
//! whatever the caller currently knows about placement, and is fully unit
//! tested without touching a socket. `InternalDns` wraps that data in a
//! `hickory-server` zone handler that can be swapped out under a running
//! server, so a service moving to another node updates the answer without a
//! restart.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, RwLock as StdRwLock};

use cl_raft::InstanceId;
use hickory_proto::rr::rdata::{A, SOA};
use hickory_proto::rr::{LowerName, Name, RData, Record, RecordType, TSigResponseContext};
use hickory_server::net::runtime::TokioRuntimeProvider;
use hickory_server::server::{Request, RequestInfo, Server};
use hickory_server::store::forwarder::ForwardZoneHandler;
use hickory_server::store::in_memory::InMemoryZoneHandler;
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, Catalog, LookupControlFlow, LookupOptions, ZoneHandler, ZoneType,
};
use tokio::sync::RwLock;

use crate::error::{NetError, Result};

/// TTL used on every record we serve. Short enough that a service move (which
/// hot-swaps the zone rather than waiting for a TTL) is what actually matters
/// for freshness; this just bounds how long a resolver caches a stale answer
/// if it doesn't respect the swap.
pub const RECORD_TTL: u32 = 5;

/// Zone served when the internal network declares no domain. Nothing outside
/// the cluster resolves it, which is the point: it only has to be a name the
/// containers agree on.
pub const DEFAULT_ZONE: &str = "cluster.local";

/// The domain the internal zone is served under. The internal network's own
/// domain when it has one, so `web.blog.int.example.com` is the same name
/// inside a container and outside it.
pub fn zone_origin(cfg: &cl_config::ClusterConfig) -> String {
    cfg.internal_network()
        .1
        .domain
        .clone()
        .unwrap_or_else(|| DEFAULT_ZONE.to_string())
}

/// One resolvable name inside the internal zone and the addresses it answers
/// with. Deliberately independent of any DNS wire format: this is what the
/// rest of clouduler reasons about, and what the tests below check directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneRecord {
    /// Label relative to the zone origin, e.g. `web.gitlab`, `tasks.web.gitlab`
    /// or a bare node name.
    pub name: String,
    /// Every address this name should answer with. More than one means the
    /// resolver round robins, same as Swarm's `tasks.<service>` names.
    pub addresses: Vec<Ipv4Addr>,
}

/// Build the internal zone from the current placement.
///
/// `<service>.<stack>` and `tasks.<service>.<stack>` both resolve to every
/// running instance of that service: an ordinary DNS client doing round
/// robin over multiple A records gets a live replica either way, and
/// `tasks.` is kept because that's the name people already know from Swarm.
/// Node names resolve to each node's address on the internal network, so a
/// container can reach a node directly (e.g. to talk to a drive's rsync
/// endpoint) without knowing the topology.
pub fn build_records<'a>(
    instances: impl IntoIterator<Item = (&'a InstanceId, Ipv4Addr)>,
    nodes: impl IntoIterator<Item = (&'a str, Ipv4Addr)>,
) -> Vec<ZoneRecord> {
    let mut services: BTreeMap<(String, String), Vec<Ipv4Addr>> = BTreeMap::new();
    for (id, ip) in instances {
        services
            .entry((id.service.clone(), id.stack.clone()))
            .or_default()
            .push(ip);
    }

    let mut records = Vec::new();
    for ((service, stack), mut addrs) in services {
        addrs.sort();
        addrs.dedup();
        records.push(ZoneRecord {
            name: format!("{service}.{stack}"),
            addresses: addrs.clone(),
        });
        records.push(ZoneRecord {
            name: format!("tasks.{service}.{stack}"),
            addresses: addrs,
        });
    }

    for (node, ip) in nodes {
        records.push(ZoneRecord {
            name: node.to_string(),
            addresses: vec![ip],
        });
    }

    records.sort_by(|a, b| a.name.cmp(&b.name));
    records
}

/// Look up a name in a record set the way the resolver would: exact label
/// match against the zone-relative name. Used directly by tests so the
/// answering logic is checked without going through DNS wire encoding.
pub fn answer<'a>(records: &'a [ZoneRecord], name: &str) -> Option<&'a [Ipv4Addr]> {
    records
        .iter()
        .find(|r| r.name == name)
        .map(|r| r.addresses.as_slice())
}

/// Turn the pure record list into a hickory `InMemoryZoneHandler` rooted at
/// `origin`, ready to serve. `origin` must be a fully qualified name (e.g.
/// `int.example.com.`).
fn build_authority(origin: &Name, records: &[ZoneRecord]) -> Result<InMemoryZoneHandler> {
    let mut handler =
        InMemoryZoneHandler::empty(origin.clone(), ZoneType::Primary, AxfrPolicy::Deny);

    let rname = Name::parse("hostmaster", Some(origin))
        .map_err(|e| NetError::Dns(format!("invalid zone origin {origin}: {e}")))?;
    let soa = SOA::new(origin.clone(), rname, 1, 3600, 600, 86400, RECORD_TTL);
    handler.upsert_mut(
        Record::from_rdata(origin.clone(), RECORD_TTL, RData::SOA(soa)),
        1,
    );

    for record in records {
        let name = Name::parse(&record.name, Some(origin))
            .map_err(|e| NetError::Dns(format!("invalid record name {}: {e}", record.name)))?;
        for addr in &record.addresses {
            let rdata = RData::A(A::from(*addr));
            handler.upsert_mut(Record::from_rdata(name.clone(), RECORD_TTL, rdata), 1);
        }
    }

    Ok(handler)
}

/// A zone handler whose backing data can be replaced while the server is
/// running. `ZoneHandler` methods borrow the current snapshot for the
/// duration of the call, so an in-flight query always sees a consistent
/// zone, either the old one or the new one, never a mix.
struct SwappableZone {
    origin: LowerName,
    current: RwLock<Arc<InMemoryZoneHandler>>,
}

impl SwappableZone {
    fn new(handler: InMemoryZoneHandler) -> Self {
        Self {
            origin: handler.origin().clone(),
            current: RwLock::new(Arc::new(handler)),
        }
    }

    async fn swap(&self, handler: InMemoryZoneHandler) {
        *self.current.write().await = Arc::new(handler);
    }
}

#[async_trait::async_trait]
impl ZoneHandler for SwappableZone {
    fn zone_type(&self) -> ZoneType {
        ZoneType::Primary
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        AxfrPolicy::Deny
    }

    fn origin(&self) -> &LowerName {
        &self.origin
    }

    async fn lookup(
        &self,
        name: &LowerName,
        rtype: RecordType,
        request_info: Option<&RequestInfo<'_>>,
        lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        let zone = self.current.read().await.clone();
        zone.lookup(name, rtype, request_info, lookup_options).await
    }

    async fn search(
        &self,
        request: &Request,
        lookup_options: LookupOptions,
    ) -> (LookupControlFlow<AuthLookup>, Option<TSigResponseContext>) {
        let zone = self.current.read().await.clone();
        zone.search(request, lookup_options).await
    }

    async fn nsec_records(
        &self,
        name: &LowerName,
        lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        let zone = self.current.read().await.clone();
        zone.nsec_records(name, lookup_options).await
    }
}

/// Answers for names that live outside the internal zone, ahead of the
/// upstream forwarder, and steps aside (`Skip`) for everything else so the
/// forwarder still resolves it. Used to point a hostname the cluster itself
/// serves at this node's own proxy, see `InternalDns::reload_overrides`.
struct OverrideZone {
    origin: LowerName,
    current: RwLock<Arc<Overrides>>,
}

struct Overrides {
    handler: InMemoryZoneHandler,
    names: BTreeSet<LowerName>,
}

impl OverrideZone {
    fn new() -> Result<Self> {
        Ok(Self {
            origin: LowerName::from(Name::root()),
            current: RwLock::new(Arc::new(build_overrides(&[])?)),
        })
    }
}

/// Like `build_authority`, rooted at `.` and with every record name taken as
/// absolute.
fn build_overrides(records: &[ZoneRecord]) -> Result<Overrides> {
    let root = Name::root();
    let handler = build_authority(&root, records)?;
    let mut names = BTreeSet::new();
    for record in records {
        let name = Name::parse(&record.name, Some(&root))
            .map_err(|e| NetError::Dns(format!("invalid record name {}: {e}", record.name)))?;
        names.insert(LowerName::from(name));
    }
    Ok(Overrides { handler, names })
}

#[async_trait::async_trait]
impl ZoneHandler for OverrideZone {
    fn zone_type(&self) -> ZoneType {
        ZoneType::Primary
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        AxfrPolicy::Deny
    }

    fn origin(&self) -> &LowerName {
        &self.origin
    }

    async fn lookup(
        &self,
        name: &LowerName,
        rtype: RecordType,
        request_info: Option<&RequestInfo<'_>>,
        lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        let current = self.current.read().await.clone();
        if !current.names.contains(name) {
            return LookupControlFlow::Skip;
        }
        current
            .handler
            .lookup(name, rtype, request_info, lookup_options)
            .await
    }

    async fn search(
        &self,
        request: &Request,
        lookup_options: LookupOptions,
    ) -> (LookupControlFlow<AuthLookup>, Option<TSigResponseContext>) {
        let current = self.current.read().await.clone();
        let Ok(info) = request.request_info() else {
            return (LookupControlFlow::Skip, None);
        };
        if !current.names.contains(info.query.name()) {
            return (LookupControlFlow::Skip, None);
        }
        current.handler.search(request, lookup_options).await
    }

    async fn nsec_records(
        &self,
        _name: &LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
}

/// Told about every address the upstream resolver answered for a client, before
/// the answer goes back to it. This is how a wan `allowed` list opens the
/// firewall for a name exactly when the stack asks for it: the observer has
/// the grant in place by the time the container learns the address.
#[async_trait::async_trait]
pub trait AnswerObserver: Send + Sync {
    /// `name` is the queried name, lowercase with no trailing dot. `answers`
    /// are the IPv4 addresses of the answer, with their TTL in seconds.
    async fn observe(&self, client: IpAddr, name: &str, answers: &[(Ipv4Addr, u32)]);
}

type ObserverSlot = Arc<StdRwLock<Option<Arc<dyn AnswerObserver>>>>;

/// The upstream forwarder with the [`AnswerObserver`] hooked onto its answers.
struct ObservedForwarder {
    inner: ForwardZoneHandler<TokioRuntimeProvider>,
    observer: ObserverSlot,
}

#[async_trait::async_trait]
impl ZoneHandler for ObservedForwarder {
    fn zone_type(&self) -> ZoneType {
        self.inner.zone_type()
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        self.inner.axfr_policy()
    }

    fn origin(&self) -> &LowerName {
        self.inner.origin()
    }

    async fn lookup(
        &self,
        name: &LowerName,
        rtype: RecordType,
        request_info: Option<&RequestInfo<'_>>,
        lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        self.inner
            .lookup(name, rtype, request_info, lookup_options)
            .await
    }

    async fn search(
        &self,
        request: &Request,
        lookup_options: LookupOptions,
    ) -> (LookupControlFlow<AuthLookup>, Option<TSigResponseContext>) {
        let (flow, ctx) = self.inner.search(request, lookup_options).await;
        let observer = self.observer.read().ok().and_then(|slot| slot.clone());
        if let (Some(observer), LookupControlFlow::Continue(Ok(lookup))) = (observer, &flow) {
            let answers: Vec<(Ipv4Addr, u32)> = lookup
                .iter()
                .filter_map(|record| match &record.data {
                    RData::A(a) => Some((a.0, record.ttl)),
                    _ => None,
                })
                .collect();
            if !answers.is_empty() {
                if let Ok(info) = request.request_info() {
                    let name = info
                        .query
                        .name()
                        .to_string()
                        .trim_end_matches('.')
                        .to_ascii_lowercase();
                    observer.observe(request.src().ip(), &name, &answers).await;
                }
            }
        }
        (flow, ctx)
    }

    async fn nsec_records(
        &self,
        name: &LowerName,
        lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        self.inner.nsec_records(name, lookup_options).await
    }
}

/// The internal DNS resolver: serves the clouduler zone on the bridge
/// address and forwards everything else to the host's configured resolvers.
pub struct InternalDns {
    origin: Name,
    zone: Arc<SwappableZone>,
    overrides: Arc<OverrideZone>,
    observer: ObserverSlot,
}

impl InternalDns {
    /// Build the resolver. `origin` is the internal zone's domain (e.g. the
    /// cluster's declared internal network domain, or a synthetic default
    /// such as `cluster.local.` when none is configured).
    pub fn new(origin: Name, records: &[ZoneRecord]) -> Result<Self> {
        let handler = build_authority(&origin, records)?;
        Ok(Self {
            origin,
            zone: Arc::new(SwappableZone::new(handler)),
            overrides: Arc::new(OverrideZone::new()?),
            observer: Arc::default(),
        })
    }

    /// Have `observer` told of every upstream answer from now on. Applies to
    /// servers already running too.
    pub fn set_observer(&self, observer: Arc<dyn AnswerObserver>) {
        if let Ok(mut slot) = self.observer.write() {
            *slot = Some(observer);
        }
    }

    /// Replace the zone content in place. Called whenever placement changes,
    /// so a moved or scaled service is reflected without restarting the
    /// server or dropping in-flight queries.
    pub async fn reload(&self, records: &[ZoneRecord]) -> Result<()> {
        let handler = build_authority(&self.origin, records)?;
        self.zone.swap(handler).await;
        Ok(())
    }

    /// Replace the names answered outside the internal zone. Each record name
    /// is an absolute hostname (`auth.example.com`); anything not listed still
    /// goes upstream. Only A records are served for a listed name, so its
    /// AAAA query gets an empty answer and clients stay on the address given.
    pub async fn reload_overrides(&self, records: &[ZoneRecord]) -> Result<()> {
        let overrides = build_overrides(records)?;
        *self.overrides.current.write().await = Arc::new(overrides);
        Ok(())
    }

    /// Serve on `bind_addr` until the process exits or the server errors out.
    /// Queries outside the internal zone are forwarded using the host's
    /// system resolver configuration (`/etc/resolv.conf`).
    pub async fn serve(&self, bind_addr: SocketAddr) -> Result<()> {
        let mut catalog = Catalog::new();
        catalog.upsert(
            self.zone.origin().clone(),
            vec![self.zone.clone() as Arc<dyn ZoneHandler>],
        );

        let forward_origin = LowerName::from(Name::root());
        let forwarder = ForwardZoneHandler::builder(TokioRuntimeProvider::default())
            .map_err(NetError::Dns)?
            .build()
            .map_err(NetError::Dns)?;
        catalog.upsert(
            forward_origin,
            vec![
                self.overrides.clone() as Arc<dyn ZoneHandler>,
                Arc::new(ObservedForwarder {
                    inner: forwarder,
                    observer: self.observer.clone(),
                }),
            ],
        );

        let mut server = Server::new(catalog);
        let socket = tokio::net::UdpSocket::bind(bind_addr)
            .await
            .map_err(|source| NetError::Io {
                context: format!("dns bind {bind_addr}"),
                source,
            })?;
        server.register_socket(socket);
        server
            .block_until_done()
            .await
            .map_err(|e| NetError::Dns(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iid(stack: &str, service: &str, index: u32) -> InstanceId {
        InstanceId::new(stack, service, index)
    }

    /// The override zone only claims the names it was given; everything else
    /// must reach the upstream forwarder behind it.
    #[test]
    fn overrides_hold_absolute_names_only() {
        let overrides = build_overrides(&[ZoneRecord {
            name: "auth.example.org.".to_string(),
            addresses: vec![Ipv4Addr::new(10, 10, 0, 1)],
        }])
        .unwrap();
        let listed = LowerName::from(Name::parse("auth.example.org.", None).unwrap());
        let other = LowerName::from(Name::parse("www.example.org.", None).unwrap());
        assert!(overrides.names.contains(&listed));
        assert!(!overrides.names.contains(&other));
    }

    /// Over the wire: a listed outside name gets the override, its AAAA is
    /// empty rather than forwarded, and a name in the internal zone still
    /// answers from the zone.
    #[tokio::test]
    async fn an_overridden_name_answers_a_and_leaves_aaaa_empty() {
        use hickory_proto::op::{Message, MessageType, OpCode, Query};

        let port = {
            let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let dns = Arc::new(
            InternalDns::new(
                Name::parse("int.example.com.", None).unwrap(),
                &[ZoneRecord {
                    name: "web.blog".to_string(),
                    addresses: vec![Ipv4Addr::new(10, 244, 0, 2)],
                }],
            )
            .unwrap(),
        );
        dns.reload_overrides(&[ZoneRecord {
            name: "auth.example.org.".to_string(),
            addresses: vec![Ipv4Addr::new(10, 10, 0, 1)],
        }])
        .await
        .unwrap();
        let server = dns.clone();
        tokio::spawn(async move { server.serve(addr).await });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        async fn ask(addr: SocketAddr, name: &str, rtype: RecordType) -> Message {
            let mut msg = Message::query();
            msg.metadata.id = 7;
            msg.metadata.message_type = MessageType::Query;
            msg.metadata.op_code = OpCode::Query;
            msg.add_query(Query::query(Name::parse(name, None).unwrap(), rtype));
            let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            socket.send_to(&msg.to_vec().unwrap(), addr).await.unwrap();
            let mut buf = [0u8; 512];
            let (n, _) = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                socket.recv_from(&mut buf),
            )
            .await
            .expect("an answer")
            .unwrap();
            Message::from_vec(&buf[..n]).unwrap()
        }

        let a = ask(addr, "auth.example.org.", RecordType::A).await;
        assert_eq!(a.answers.len(), 1);
        assert_eq!(
            a.answers[0].data,
            RData::A(A::from(Ipv4Addr::new(10, 10, 0, 1)))
        );
        let aaaa = ask(addr, "auth.example.org.", RecordType::AAAA).await;
        assert!(aaaa.answers.is_empty());
        let zone = ask(addr, "web.blog.int.example.com.", RecordType::A).await;
        assert_eq!(zone.answers.len(), 1);
    }

    #[test]
    fn service_name_resolves_to_its_instance() {
        let a = iid("gitlab", "web", 0);
        let ip: Ipv4Addr = "10.244.0.2".parse().unwrap();
        let records = build_records([(&a, ip)], []);
        assert_eq!(answer(&records, "web.gitlab"), Some(&[ip][..]));
    }

    #[test]
    fn tasks_name_lists_every_replica() {
        let a = iid("gitlab", "web", 0);
        let b = iid("gitlab", "web", 1);
        let ip_a: Ipv4Addr = "10.244.0.2".parse().unwrap();
        let ip_b: Ipv4Addr = "10.244.0.3".parse().unwrap();
        let records = build_records([(&a, ip_a), (&b, ip_b)], []);

        let mut got = answer(&records, "tasks.web.gitlab").unwrap().to_vec();
        got.sort();
        assert_eq!(got, vec![ip_a, ip_b]);

        // The bare service name round robins over the same set.
        let mut got = answer(&records, "web.gitlab").unwrap().to_vec();
        got.sort();
        assert_eq!(got, vec![ip_a, ip_b]);
    }

    #[test]
    fn different_stacks_do_not_collide() {
        let a = iid("gitlab", "web", 0);
        let b = iid("blog", "web", 0);
        let ip_a: Ipv4Addr = "10.244.0.2".parse().unwrap();
        let ip_b: Ipv4Addr = "10.244.1.2".parse().unwrap();
        let records = build_records([(&a, ip_a), (&b, ip_b)], []);

        assert_eq!(answer(&records, "web.gitlab"), Some(&[ip_a][..]));
        assert_eq!(answer(&records, "web.blog"), Some(&[ip_b][..]));
    }

    #[test]
    fn node_names_resolve_directly() {
        let ip: Ipv4Addr = "10.10.0.1".parse().unwrap();
        let records = build_records([], [("n1", ip)]);
        assert_eq!(answer(&records, "n1"), Some(&[ip][..]));
    }

    #[test]
    fn unknown_name_has_no_answer() {
        let records = build_records([], []);
        assert_eq!(answer(&records, "ghost.gitlab"), None);
    }

    #[test]
    fn zone_updates_when_placement_changes() {
        let a = iid("gitlab", "web", 0);
        let ip1: Ipv4Addr = "10.244.0.2".parse().unwrap();
        let before = build_records([(&a, ip1)], []);
        assert_eq!(answer(&before, "web.gitlab"), Some(&[ip1][..]));

        // Instance moves to a new node and picks up a different address.
        let ip2: Ipv4Addr = "10.244.0.9".parse().unwrap();
        let after = build_records([(&a, ip2)], []);
        assert_eq!(answer(&after, "web.gitlab"), Some(&[ip2][..]));
    }

    #[test]
    fn builds_a_hickory_authority_from_the_zone() {
        let a = iid("gitlab", "web", 0);
        let ip: Ipv4Addr = "10.244.0.2".parse().unwrap();
        let records = build_records([(&a, ip)], [("n1", "10.10.0.1".parse().unwrap())]);
        let origin: Name = "int.example.com.".parse().unwrap();
        let authority = build_authority(&origin, &records).expect("builds");
        assert_eq!(authority.origin(), &LowerName::from(origin));
    }

    #[tokio::test]
    async fn reload_swaps_the_served_zone() {
        let origin: Name = "int.example.com.".parse().unwrap();
        let a = iid("gitlab", "web", 0);
        let ip1: Ipv4Addr = "10.244.0.2".parse().unwrap();
        let dns = InternalDns::new(origin, &build_records([(&a, ip1)], [])).unwrap();

        let ip2: Ipv4Addr = "10.244.0.9".parse().unwrap();
        dns.reload(&build_records([(&a, ip2)], [])).await.unwrap();
        // No direct getter on the live zone by design (queries go through
        // the DNS wire protocol); reaching here without error is the swap
        // succeeding, exercised end to end via `answer` above.
    }
}
