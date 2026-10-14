//! Active backend health checks.
//!
//! The routing table alone can't tell "nobody has hit this route yet" apart
//! from "the next real request would 502" — a route sits idle between
//! deploys as easily as a genuinely broken one does. This probes every
//! route's backends itself on a timer and remembers what it found, so
//! `clouduler stack unhealthy` and the ui can surface a dead backend before a
//! client discovers it the hard way.
//!
//! Deliberately independent of the request path in `server.rs`: reusing that
//! code would mean synthesizing a fake client request, and the failure modes
//! this cares about (connect refused, timeout, TLS on a route that expects
//! plain HTTP) are exactly what a plain probe request already reproduces.
//! It does dial the way the request path does, though, through the same
//! `cl_net::relay::Dialer`: a backend on another node is probed through the
//! relay of the node running it, since this host cannot reach it over the
//! overlay at all.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use cl_net::relay::Dialer;
use http::header::HOST;
use http::{Request, Uri};
use http_body_util::Empty;
use hyper_util::rt::TokioIo;
use tracing::debug;

use crate::route::{Route, RouteTable};

/// How long a single backend probe has to connect and get a response before
/// it's counted as unreachable. Generous relative to `CONNECT_TIMEOUT` since
/// this also waits for a full response.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a probe waits for the backend to accept, through its node's relay
/// included. Matches `BACKEND_CONNECT_TIMEOUT` in `server.rs`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often every route in the table gets re-probed.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// One route's most recent probe.
#[derive(Debug, Clone)]
pub struct BackendHealth {
    pub host: String,
    pub checked_at: SystemTime,
    /// `None` when the probe reached a backend and got a response, whatever
    /// its status; `Some(reason)` in the same shape a real client would have
    /// seen from the proxy's own `bad gateway: {reason}` response.
    pub error: Option<String>,
}

impl BackendHealth {
    pub fn healthy(&self) -> bool {
        self.error.is_none()
    }
}

/// Latest probe result per route, keyed by `Route::instance`. Cheap to clone:
/// every clone shares the same table.
#[derive(Clone, Default)]
pub struct HealthTable(Arc<Mutex<HashMap<String, BackendHealth>>>);

impl HealthTable {
    pub fn get(&self, instance: &str) -> Option<BackendHealth> {
        self.0
            .lock()
            .expect("proxy health lock")
            .get(instance)
            .cloned()
    }

    fn record(&self, instance: String, health: BackendHealth) {
        self.0
            .lock()
            .expect("proxy health lock")
            .insert(instance, health);
    }

    /// Drops entries for routes no longer in the table, so a removed stack
    /// doesn't linger as a stale unhealthy row forever.
    fn retain_known(&self, known: &std::collections::HashSet<String>) {
        self.0
            .lock()
            .expect("proxy health lock")
            .retain(|k, _| known.contains(k));
    }
}

/// Probes every route in `routes` every `interval`, forever.
///
/// `tokio::time::interval` fires its first tick immediately, which would mean
/// probing the whole cluster's routes the instant this node's proxy comes
/// up - exactly when the VXLAN FDB and ARP caches are coldest and a probe is
/// most likely to eat a one-off timeout that has nothing to do with the
/// backend actually being down. Waiting out one interval before the first
/// pass gives the overlay a chance to warm up first.
///
/// Backends are dialed through `dialer`, as the proxy itself dials them.
pub async fn run(routes: Arc<RouteTable>, health: HealthTable, interval: Duration, dialer: Dialer) {
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let all = routes.all();
        let known = all.iter().map(|r| r.instance.clone()).collect();
        health.retain_known(&known);

        for route in all {
            // Redirects and routes with no configured backends (a redirect
            // rule, an unresolved static host) never proxy anywhere, so
            // there's nothing to probe.
            if route.redirect.is_some() || route.backends().is_empty() {
                continue;
            }
            // On its own task so a panic in one route's probe (a reqwest
            // internal bug, an unexpected header value, anything) can't take
            // this loop down with it. Losing the loop wedges every route's
            // health at whatever it last recorded, forever — confirmed live:
            // one route's `BackendHealth` sat unchanged for over an hour
            // (many `interval`s) reporting an address the backend hadn't
            // held since its last restart, and only a process restart
            // cleared it. `spawn` isolates the panic to this route's probe.
            let instance = route.instance.clone();
            let dialer = dialer.clone();
            let health = health.clone();
            tokio::spawn(async move {
                let result = probe_route(&dialer, &route).await;
                health.record(instance, result);
            });
        }
    }
}

/// A failed probe gets this many extra attempts before the backend is
/// actually recorded as unreachable. Head-end replication over the VXLAN
/// mesh (see `cl-net::vxlan`) means a cross-node probe occasionally eats a
/// one-off dropped packet on an otherwise healthy backend - confirmed live:
/// a route that failed its first post-restart probe came back healthy on
/// the very next attempt seconds later, with nothing on the backend having
/// changed. Retrying in-place tells that apart from a backend that is
/// actually down, which keeps failing every attempt.
const PROBE_RETRIES: u32 = 2;

/// Wait between retry attempts on the same backend.
const RETRY_DELAY: Duration = Duration::from_millis(500);

/// Probes every backend behind one route; the route is healthy only if all
/// of them are, since a client can land on any of them.
async fn probe_route(dialer: &Dialer, route: &Route) -> BackendHealth {
    let mut error = None;
    // Probe the route's own `path_prefix` rather than always `/`: a backend
    // mounted under a prefix (headplane's react-router basename is
    // `/admin/`, matching its route's `path_prefix`) has no route at all for
    // bare `/` and answers with its own "no route matches" error, which read
    // as the backend being unhealthy when it was actually fine.
    let path = route.path_prefix.as_deref().unwrap_or("/");
    for addr in route.backends() {
        let mut last_error = None;
        for attempt in 0..=PROBE_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(RETRY_DELAY).await;
            }
            match probe(dialer, *addr, &route.host, path).await {
                Ok(()) => {
                    last_error = None;
                    break;
                }
                Err(e) => {
                    debug!(host = %route.host, %addr, attempt, error = %e, "proxy health check attempt failed");
                    last_error = Some(e);
                }
            }
        }
        if let Some(e) = last_error {
            error = Some(format!("{addr} unreachable: {e}"));
            break;
        }
    }
    BackendHealth {
        host: route.host.clone(),
        checked_at: SystemTime::now(),
        error,
    }
}

/// One probe request to one backend: a `GET path` with the route's `Host`,
/// counted as reachable once any response comes back, whatever its status
/// (a redirect is not followed - it already proves the backend answers).
///
/// The proxy always speaks plain HTTP to a backend regardless of whether the
/// route terminates TLS for clients (see `server.rs`). Every probe gets a
/// fresh connection: probes for one backend are `interval` apart - far too
/// infrequent for a pooled connection to earn its keep, and confirmed live to
/// actively hurt: a backend whose first probe landed mid-startup (TCP
/// accepted, then reset before a response) left a dead connection in the pool
/// that every retry and every later tick kept reusing, reporting the same
/// route unreachable indefinitely even after a plain `curl` to the identical
/// address succeeded instantly.
async fn probe(dialer: &Dialer, addr: SocketAddr, host: &str, path: &str) -> Result<(), String> {
    let uri: Uri = path
        .parse()
        .map_err(|e| format!("probe path {path:?} is not a valid request target: {e}"))?;
    let request = Request::get(uri)
        .header(HOST, host)
        .body(Empty::<Bytes>::new())
        .map_err(|e| format!("building the probe request: {e}"))?;
    let exchange = async {
        let stream = dialer
            .connect_tcp(addr, CONNECT_TIMEOUT)
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| e.to_string())?;
        // The connection is driven right here, alongside the request, and
        // dropped (closed) with it once the response head is in. It can also
        // finish first - in the very poll that hands the response over, when
        // the backend closes right after answering - and with it gone the
        // request resolves at once: to that response, or to whatever error
        // ended the connection.
        let send = sender.send_request(request);
        tokio::pin!(send, connection);
        let response = tokio::select! {
            response = &mut send => response,
            _ = &mut connection => send.await,
        };
        response.map(drop).map_err(|e| e.to_string())
    };
    tokio::time::timeout(PROBE_TIMEOUT, exchange)
        .await
        .unwrap_or_else(|_| Err(format!("no response within {}s", PROBE_TIMEOUT.as_secs())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_secret::ClusterKey;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    /// A backend that answers every request with a bare 204 and reports the
    /// request head it got.
    async fn backend() -> (SocketAddr, mpsc::UnboundedReceiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (heads, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                let heads = heads.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match conn.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let _ = heads.send(String::from_utf8_lossy(&head).into_owned());
                    let _ = conn
                        .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                        .await;
                });
            }
        });
        (addr, rx)
    }

    async fn relay(keys: cl_raft::KeyWatch) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(cl_net::relay::serve(
            listener,
            keys,
            vec!["127.0.0.0/8".parse().unwrap()],
        ));
        addr
    }

    fn free_addr() -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    }

    fn route(backend: SocketAddr, path_prefix: Option<&str>) -> Route {
        Route::new(
            "app.test",
            path_prefix.map(str::to_string),
            false,
            vec![],
            false,
            "stack/web/0",
            vec![backend],
        )
    }

    #[tokio::test]
    async fn a_local_backend_is_probed_directly_with_the_routes_host_and_prefix() {
        let (addr, mut heads) = backend().await;
        let health = probe_route(&Dialer::direct(), &route(addr, Some("/admin/"))).await;
        assert!(health.healthy(), "{:?}", health.error);
        let head = heads.recv().await.unwrap();
        assert!(head.starts_with("GET /admin/ HTTP/1.1\r\n"), "{head}");
        assert!(
            head.to_ascii_lowercase().contains("host: app.test\r\n"),
            "{head}"
        );
    }

    /// A backend on another node cannot be reached from this host over the
    /// overlay, so its probe has to go through that node's relay.
    #[tokio::test]
    async fn a_backend_on_another_node_is_probed_through_its_relay() {
        let keys = cl_raft::fixed_key(Arc::new(ClusterKey::generate()));
        let relay = relay(keys.clone()).await;
        let (addr, mut heads) = backend().await;

        let dialer = Dialer::new(keys);
        dialer.set_relays(HashMap::from([(addr.ip(), relay)]));
        let health = probe_route(&dialer, &route(addr, None)).await;
        assert!(health.healthy(), "{:?}", health.error);
        assert!(heads
            .recv()
            .await
            .unwrap()
            .starts_with("GET / HTTP/1.1\r\n"));

        // And only through it: with the relay gone the probe fails, even
        // though the backend itself is still listening.
        dialer.set_relays(HashMap::from([(addr.ip(), free_addr())]));
        let health = probe_route(&dialer, &route(addr, None)).await;
        let error = health.error.expect("unreachable through a dead relay");
        assert!(
            error.starts_with(&format!("{addr} unreachable: ")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_backend_that_refuses_is_unhealthy() {
        let dead = free_addr();
        let health = probe_route(&Dialer::direct(), &route(dead, None)).await;
        let error = health.error.expect("nothing listens there");
        assert!(
            error.starts_with(&format!("{dead} unreachable: connect: ")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_backend_that_hangs_up_without_answering_is_unhealthy() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                drop(conn);
            }
        });
        let health = probe_route(&Dialer::direct(), &route(addr, None)).await;
        assert!(!health.healthy());
    }
}
