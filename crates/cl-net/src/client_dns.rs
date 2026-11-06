//! The client-facing DNS server: unlike the internal resolver, which answers
//! the same way for every container, this one answers differently depending on
//! which network the asking client is on.
//!
//! The caller (`cl-daemon`) decides what each *view* contains and how a client
//! address maps to a view; this module only serves them. A view holds the
//! records its clients get, the names they are refused, and whether names the
//! cluster does not own are forwarded upstream (a client inside a private
//! network) or refused (anything else, so a listener reachable from the
//! internet never becomes an open resolver).

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use hickory_proto::op::ResponseCode;
use hickory_proto::rr::rdata::{A, AAAA, CNAME, SOA};
use hickory_proto::rr::{LowerName, Name, RData, Record, TSigResponseContext};
use hickory_server::server::{Request, RequestInfo, Server};
use hickory_server::store::in_memory::InMemoryZoneHandler;
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, Catalog, LookupControlFlow, LookupError, LookupOptions, ZoneHandler,
    ZoneType,
};
use tokio::sync::RwLock;

use crate::dns::{build_split_dns_forwarder, RECORD_TTL};
use crate::error::{NetError, Result};

/// One piece of an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewAnswer {
    A(Ipv4Addr),
    Aaaa(Ipv6Addr),
    /// Target hostname, absolute.
    Cname(String),
}

/// A hostname and what a view answers for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewRecord {
    /// Absolute hostname, trailing dot optional.
    pub name: String,
    pub answers: Vec<ViewAnswer>,
}

/// What clients of one network see.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct View {
    pub records: Vec<ViewRecord>,
    /// Hostnames the cluster owns but these clients may not resolve.
    pub refused: BTreeSet<String>,
    /// Forward names nobody here owns upstream instead of refusing them.
    pub forward: bool,
}

/// Picks a view for a client address. A key with no view is refused outright.
pub type Classifier = Arc<dyn Fn(IpAddr) -> String + Send + Sync>;

struct BuiltView {
    handler: InMemoryZoneHandler,
    names: BTreeSet<LowerName>,
    refused: BTreeSet<LowerName>,
    forward: bool,
}

struct Snapshot {
    views: BTreeMap<String, BuiltView>,
    classify: Classifier,
}

fn absolute(name: &str) -> Result<Name> {
    Name::parse(name, Some(&Name::root()))
        .map_err(|e| NetError::Dns(format!("invalid record name {name}: {e}")))
}

fn build_view(view: &View) -> Result<BuiltView> {
    let root = Name::root();
    let mut handler = InMemoryZoneHandler::empty(root.clone(), ZoneType::Primary, AxfrPolicy::Deny);
    let soa = SOA::new(root.clone(), root.clone(), 1, 3600, 600, 86400, RECORD_TTL);
    handler.upsert_mut(
        Record::from_rdata(root.clone(), RECORD_TTL, RData::SOA(soa)),
        1,
    );

    let mut names = BTreeSet::new();
    for record in &view.records {
        let name = absolute(&record.name)?;
        for answer in &record.answers {
            let rdata = match answer {
                ViewAnswer::A(ip) => RData::A(A::from(*ip)),
                ViewAnswer::Aaaa(ip) => RData::AAAA(AAAA::from(*ip)),
                ViewAnswer::Cname(target) => RData::CNAME(CNAME(absolute(target)?)),
            };
            handler.upsert_mut(Record::from_rdata(name.clone(), RECORD_TTL, rdata), 1);
        }
        names.insert(LowerName::from(name));
    }
    let refused = view
        .refused
        .iter()
        .map(|n| absolute(n).map(LowerName::from))
        .collect::<Result<_>>()?;
    Ok(BuiltView {
        handler,
        names,
        refused,
        forward: view.forward,
    })
}

struct ViewZone {
    origin: LowerName,
    current: RwLock<Arc<Snapshot>>,
}

#[async_trait::async_trait]
impl ZoneHandler for ViewZone {
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
        _name: &LowerName,
        _rtype: hickory_proto::rr::RecordType,
        _request_info: Option<&RequestInfo<'_>>,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        // Every query arrives through `search`, which has the client address.
        LookupControlFlow::Skip
    }

    async fn search(
        &self,
        request: &Request,
        lookup_options: LookupOptions,
    ) -> (LookupControlFlow<AuthLookup>, Option<TSigResponseContext>) {
        let refused = || {
            (
                LookupControlFlow::Break(Err(LookupError::from(ResponseCode::Refused))),
                None,
            )
        };
        let snapshot = self.current.read().await.clone();
        let Ok(info) = request.request_info() else {
            return refused();
        };
        let key = (snapshot.classify)(request.src().ip());
        let Some(view) = snapshot.views.get(&key) else {
            return refused();
        };
        let name = info.query.name();
        if view.names.contains(name) {
            return view.handler.search(request, lookup_options).await;
        }
        if view.refused.contains(name) || !view.forward {
            return refused();
        }
        (LookupControlFlow::Skip, None)
    }

    async fn nsec_records(
        &self,
        _name: &LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
}

/// The client-facing resolver. Views can be replaced under a running server.
pub struct ClientDns {
    zone: Arc<ViewZone>,
}

impl ClientDns {
    pub fn new() -> Result<Self> {
        let snapshot = Snapshot {
            views: BTreeMap::new(),
            classify: Arc::new(|_| String::new()),
        };
        Ok(Self {
            zone: Arc::new(ViewZone {
                origin: LowerName::from(Name::root()),
                current: RwLock::new(Arc::new(snapshot)),
            }),
        })
    }

    /// Replace every view and the client classifier in one step, so a query
    /// never sees a new classifier with old views.
    pub async fn reload(&self, views: &BTreeMap<String, View>, classify: Classifier) -> Result<()> {
        let views = views
            .iter()
            .map(|(key, view)| Ok((key.clone(), build_view(view)?)))
            .collect::<Result<_>>()?;
        *self.zone.current.write().await = Arc::new(Snapshot { views, classify });
        Ok(())
    }

    /// Serve on `bind_addr` until the process exits or the server errors out.
    /// Names forwarded upstream go to `upstream`, or to the host's resolvers
    /// minus MagicDNS when it is empty.
    pub async fn serve(&self, bind_addr: SocketAddr, upstream: &[IpAddr]) -> Result<()> {
        self.serve_with(bind_addr, Arc::new(build_split_dns_forwarder(upstream)?))
            .await
    }

    async fn serve_with(
        &self,
        bind_addr: SocketAddr,
        forwarder: Arc<dyn ZoneHandler>,
    ) -> Result<()> {
        let mut catalog = Catalog::new();
        catalog.upsert(
            LowerName::from(Name::root()),
            vec![self.zone.clone() as Arc<dyn ZoneHandler>, forwarder],
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
    use hickory_proto::op::{Message, MessageType, OpCode, Query};
    use hickory_proto::rr::RecordType;

    async fn ask(addr: SocketAddr, from: &str, name: &str, rtype: RecordType) -> Message {
        let mut msg = Message::query();
        msg.metadata.id = 9;
        msg.metadata.message_type = MessageType::Query;
        msg.metadata.op_code = OpCode::Query;
        // A forwarder only answers queries that ask for recursion, as every
        // real client's do.
        msg.metadata.recursion_desired = true;
        msg.add_query(Query::query(Name::parse(name, None).unwrap(), rtype));
        let socket = tokio::net::UdpSocket::bind(format!("{from}:0"))
            .await
            .unwrap();
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

    fn a_records(msg: &Message) -> Vec<Ipv4Addr> {
        msg.answers
            .iter()
            .filter_map(|r| match &r.data {
                RData::A(a) => Some(a.0),
                _ => None,
            })
            .collect()
    }

    /// Two clients asking the same server get their own network's answer, and
    /// a name their network may not resolve is refused rather than forwarded.
    #[tokio::test]
    async fn each_client_gets_its_own_networks_answer() {
        let port = {
            let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let record = |ip: [u8; 4]| ViewRecord {
            name: "app.example.org.".into(),
            answers: vec![ViewAnswer::A(Ipv4Addr::from(ip))],
        };
        let views = BTreeMap::from([
            (
                "lan".to_string(),
                View {
                    records: vec![record([192, 168, 1, 1])],
                    refused: BTreeSet::new(),
                    forward: false,
                },
            ),
            (
                "pub".to_string(),
                View {
                    records: vec![record([203, 0, 113, 5])],
                    refused: BTreeSet::from(["secret.example.org.".to_string()]),
                    forward: false,
                },
            ),
        ]);
        let dns = Arc::new(ClientDns::new().unwrap());
        // Loopback is the "lan" client, 127.0.0.2 the "pub" one.
        let classify: Classifier = Arc::new(|ip| {
            if ip == IpAddr::from([127, 0, 0, 1]) {
                "lan"
            } else {
                "pub"
            }
            .to_string()
        });
        dns.reload(&views, classify).await.unwrap();
        let server = dns.clone();
        tokio::spawn(async move { server.serve(addr, &[]).await });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let lan = ask(addr, "127.0.0.1", "app.example.org.", RecordType::A).await;
        assert_eq!(a_records(&lan), [Ipv4Addr::new(192, 168, 1, 1)]);
        let public = ask(addr, "127.0.0.2", "app.example.org.", RecordType::A).await;
        assert_eq!(a_records(&public), [Ipv4Addr::new(203, 0, 113, 5)]);

        let denied = ask(addr, "127.0.0.2", "secret.example.org.", RecordType::A).await;
        assert_eq!(denied.metadata.response_code, ResponseCode::Refused);
        // Nobody here owns this and this view does not forward.
        let other = ask(addr, "127.0.0.2", "www.example.net.", RecordType::A).await;
        assert_eq!(other.metadata.response_code, ResponseCode::Refused);
    }

    /// A fake upstream resolver on loopback that answers every A query with
    /// `192.0.2.7` and counts what it was asked.
    async fn fake_upstream() -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = asked.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            loop {
                let Ok((n, from)) = socket.recv_from(&mut buf).await else {
                    return;
                };
                let Ok(query) = Message::from_vec(&buf[..n]) else {
                    continue;
                };
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut reply = Message::response(query.metadata.id, OpCode::Query);
                reply.metadata.response_code = ResponseCode::NoError;
                reply.metadata.recursion_desired = true;
                reply.metadata.recursion_available = true;
                for q in &query.queries {
                    reply.add_query(q.clone());
                    reply.add_answer(Record::from_rdata(
                        q.name().clone(),
                        30,
                        RData::A(A(Ipv4Addr::new(192, 0, 2, 7))),
                    ));
                }
                let _ = socket.send_to(&reply.to_vec().unwrap(), from).await;
            }
        });
        (port, asked)
    }

    /// A name nobody here owns goes to the configured upstream for a client
    /// whose view forwards, and is still refused for one whose view does not.
    #[tokio::test]
    async fn unowned_names_go_to_the_configured_upstream() {
        use hickory_server::resolver::config::{ConnectionConfig, NameServerConfig};

        let (upstream_port, asked) = fake_upstream().await;
        let mut ns = NameServerConfig::udp_and_tcp(IpAddr::from([127, 0, 0, 1]));
        ns.connections = vec![{
            let mut udp = ConnectionConfig::udp();
            udp.port = upstream_port;
            udp
        }];
        let forwarder = crate::dns::forwarder_to(vec![ns], None).unwrap();

        let port = {
            let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let views = BTreeMap::from([
            (
                "lan".to_string(),
                View {
                    forward: true,
                    ..View::default()
                },
            ),
            ("pub".to_string(), View::default()),
        ]);
        let dns = Arc::new(ClientDns::new().unwrap());
        let classify: Classifier = Arc::new(|ip| {
            if ip == IpAddr::from([127, 0, 0, 1]) {
                "lan"
            } else {
                "pub"
            }
            .to_string()
        });
        dns.reload(&views, classify).await.unwrap();
        let server = dns.clone();
        tokio::spawn(async move { server.serve_with(addr, Arc::new(forwarder)).await });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let lan = ask(addr, "127.0.0.1", "mail.example.net.", RecordType::A).await;
        assert_eq!(a_records(&lan), [Ipv4Addr::new(192, 0, 2, 7)]);
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);

        let outsider = ask(addr, "127.0.0.2", "mail.example.net.", RecordType::A).await;
        assert_eq!(outsider.metadata.response_code, ResponseCode::Refused);
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
