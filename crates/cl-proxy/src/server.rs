//! The hyper 1.x listeners: plain HTTP (redirect or proxy) and HTTPS
//! (TLS terminated here, then proxied).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use cl_net::relay::Dialer;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderValue, HOST};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Version};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use rustls::server::Acceptor;
use rustls::ServerConfig;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::LazyConfigAcceptor;
use tracing::{debug, warn};

use crate::allow::PeerPolicies;
use crate::auth_gate::{AuthGate, GateOutcome};
use crate::certs::{ensure_crypto_provider, CertResolver, CertStore};
use crate::connector::BackendConnector;
use crate::error::Result;
use crate::http01::Http01Store;
use crate::middleware;
use crate::route::{RedirectTarget, Route, RouteTable, STICKY_COOKIE};

/// Path prefix ACME HTTP-01 validators request; the rest of the path is the
/// challenge token. Answered ahead of routing and regardless of `Host`,
/// since the CA's validator has no reason to send one clouduler would
/// recognize.
const ACME_CHALLENGE_PREFIX: &str = "/.well-known/acme-challenge/";

type BoxError = Box<dyn std::error::Error + Send + Sync>;
/// The single body type every response and outbound request in this crate
/// uses, so both listeners and the backend client agree on one shape.
pub type ProxyBody = BoxBody<Bytes, BoxError>;

pub(crate) fn empty_body() -> ProxyBody {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

pub(crate) fn text_body(s: impl Into<Bytes>) -> ProxyBody {
    Full::new(s.into()).map_err(|never| match never {}).boxed()
}

fn incoming_to_proxy_body(b: Incoming) -> ProxyBody {
    b.map_err(|e| Box::new(e) as BoxError).boxed()
}

/// The outbound clients and gate a request may need once a route is found,
/// bundled together so passing them down to a connection handler is one
/// argument instead of three.
#[derive(Clone)]
struct Backends {
    client: Client<BackendConnector, ProxyBody>,
    /// Same dialer, but speaks h2c (HTTP/2 with prior knowledge); used by
    /// routes with `http2` set.
    client_h2: Client<BackendConnector, ProxyBody>,
    /// What `client` dials through, for the websocket path, which opens its
    /// own connection outside the pool.
    dialer: Dialer,
    auth_gate: Option<Arc<AuthGate>>,
    http01: Arc<Http01Store>,
    peers: Arc<PeerPolicies>,
}

/// Runs the plain HTTP and TLS listeners for one routing table and cert store.
pub struct ProxyServer {
    routes: Arc<RouteTable>,
    certs: Arc<CertStore>,
    client: Client<BackendConnector, ProxyBody>,
    client_h2: Client<BackendConnector, ProxyBody>,
    /// How backends are dialed: directly, or through the relay of the node
    /// running a container this node does not (see `with_dialer`).
    dialer: Dialer,
    /// `None` when nothing needs it yet: only built once a route actually
    /// carries the `authenticated` middleware.
    auth_gate: Option<Arc<AuthGate>>,
    /// ACME HTTP-01 challenge responses, served out of the plain HTTP
    /// listener regardless of which route (if any) matches the request host.
    http01: Arc<Http01Store>,
    /// Per-stack host allow lists, shared with whatever keeps them current.
    peers: Arc<PeerPolicies>,
}

/// How long a backend gets to accept a TCP connection before it's treated as
/// unreachable, through its node's relay included. Without this, a dial
/// falls back to the OS's own SYN retry timeout (minutes, not seconds) on a
/// backend that never sends a SYN-ACK or RST, or on a relay whose node just
/// died, instead of failing fast.
const BACKEND_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on how long a backend has to return response headers when the route
/// itself doesn't configure `response_timeout`. `RouteOptions::response_timeout`
/// defaults to `None` (unbounded) - fine when the backend is always reachable
/// instantly, not fine as the only thing standing between a stalled backend
/// and a request that never completes.
const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a pooled backend connection may sit idle before this client
/// proactively drops it instead of reusing it. Without this,
/// `hyper_util`'s legacy pool only notices a dead connection when it tries
/// to poll it ready for reuse, which detects a graceful close but not a
/// hard reset - a container that gets killed (not stopped gracefully) while
/// a keep-alive connection to it is idle in the pool resets the socket
/// instead of closing it, and the next real request to reuse that
/// connection fails outright with a `SendRequest` error (confirmed live on
/// `stalwart`, a backend that restarts in well under 30s and was otherwise
/// completely healthy). Keeping the idle budget short bounds how long a
/// connection can go stale in the pool before it's evicted.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// The pooled client every proxied request goes out on.
fn backend_client(dialer: Dialer) -> Client<BackendConnector, ProxyBody> {
    Client::builder(TokioExecutor::new())
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .build(BackendConnector::new(dialer, BACKEND_CONNECT_TIMEOUT))
}

/// How often an idle h2 connection (client or upstream) is pinged, and how
/// long the peer has to answer. A long-lived gRPC stream can sit silent for
/// minutes; without pings a NAT or a killed container leaves it half-open
/// forever.
const H2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);
const H2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Idle budget for a pooled h2c connection. Longer than `POOL_IDLE_TIMEOUT`
/// because one h2 connection carries many streams and is cheap to keep, and
/// a dead one is caught by the keep-alive pings rather than by guessing.
const H2_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The pooled client for routes with `http2` set: cleartext HTTP/2 with
/// prior knowledge, so a request is never sent as HTTP/1.1.
fn backend_client_h2(dialer: Dialer) -> Client<BackendConnector, ProxyBody> {
    Client::builder(TokioExecutor::new())
        .http2_only(true)
        .timer(TokioTimer::new())
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(H2_POOL_IDLE_TIMEOUT)
        .http2_keep_alive_interval(H2_KEEP_ALIVE_INTERVAL)
        .http2_keep_alive_timeout(H2_KEEP_ALIVE_TIMEOUT)
        .http2_keep_alive_while_idle(true)
        .build(BackendConnector::new(dialer, BACKEND_CONNECT_TIMEOUT))
}

impl ProxyServer {
    pub fn new(routes: Arc<RouteTable>, certs: Arc<CertStore>) -> Self {
        Self::with_auth_gate(routes, certs, None)
    }

    pub fn with_auth_gate(
        routes: Arc<RouteTable>,
        certs: Arc<CertStore>,
        auth_gate: Option<Arc<AuthGate>>,
    ) -> Self {
        Self::full(routes, certs, auth_gate, Arc::new(Http01Store::default()))
    }

    /// Like [`Self::with_auth_gate`], but with the ACME HTTP-01 challenge
    /// store shared with the issuance code, rather than a private one this
    /// server would never see writes to.
    pub fn full(
        routes: Arc<RouteTable>,
        certs: Arc<CertStore>,
        auth_gate: Option<Arc<AuthGate>>,
        http01: Arc<Http01Store>,
    ) -> Self {
        let dialer = Dialer::direct();
        Self {
            routes,
            certs,
            client: backend_client(dialer.clone()),
            client_h2: backend_client_h2(dialer.clone()),
            dialer,
            auth_gate,
            http01,
            peers: Arc::new(PeerPolicies::new()),
        }
    }

    /// Enforce `peers`: a caller whose stack restricted the network a route
    /// is published on gets a 403 for any host it did not list.
    pub fn with_peer_policies(mut self, peers: Arc<PeerPolicies>) -> Self {
        self.peers = peers;
        self
    }

    /// Dial backends through `dialer`, so a container another node runs is
    /// reached through that node's relay. Without it every backend is dialed
    /// directly from this host, which only reaches containers running here.
    pub fn with_dialer(mut self, dialer: Dialer) -> Self {
        self.client = backend_client(dialer.clone());
        self.client_h2 = backend_client_h2(dialer.clone());
        self.dialer = dialer;
        self
    }

    fn backends(&self) -> Backends {
        Backends {
            client: self.client.clone(),
            client_h2: self.client_h2.clone(),
            dialer: self.dialer.clone(),
            auth_gate: self.auth_gate.clone(),
            http01: self.http01.clone(),
            peers: self.peers.clone(),
        }
    }

    /// Serves plain HTTP forever: 308-redirects tls routes to https, proxies
    /// everything else directly.
    pub async fn serve_http(&self, addr: SocketAddr) -> Result<()> {
        let listener = TcpListener::bind(addr).await?;
        self.serve_http_on(listener).await
    }

    /// Like `serve_http`, on a listener the caller already bound. Lets a
    /// restarting daemon claim its port before it is ready to answer: clients
    /// queue in the backlog instead of being refused.
    pub async fn serve_http_on(&self, listener: TcpListener) -> Result<()> {
        debug!(addr = ?listener.local_addr().ok(), "http listener started");
        loop {
            let (stream, peer) = listener.accept().await?;
            let routes = self.routes.clone();
            let backends = self.backends();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let svc =
                    service_fn(move |req| handle_http(req, routes.clone(), backends.clone(), peer));
                if let Err(e) = http1::Builder::new()
                    .serve_connection(io, svc)
                    .with_upgrades()
                    .await
                {
                    debug!(error = %e, "http connection closed");
                }
            });
        }
    }

    /// Serves HTTPS forever. One shared `rustls::ServerConfig` resolves the
    /// certificate per connection by SNI via `CertResolver`; before completing
    /// the handshake we peek the SNI name and warm the cert store's
    /// self-signed fallback cache for it, since `ResolvesServerCert::resolve`
    /// is synchronous and can't generate one itself on a first connection.
    pub async fn serve_https(&self, addr: SocketAddr) -> Result<()> {
        let listener = TcpListener::bind(addr).await?;
        self.serve_https_on(listener).await
    }

    /// Like `serve_https`, on a listener the caller already bound.
    pub async fn serve_https_on(&self, listener: TcpListener) -> Result<()> {
        ensure_crypto_provider();
        let resolver = Arc::new(CertResolver::new(self.certs.clone()));
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(resolver);
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let config = Arc::new(config);

        debug!(addr = ?listener.local_addr().ok(), "https listener started");
        loop {
            let (stream, peer) = listener.accept().await?;
            let routes = self.routes.clone();
            let backends = self.backends();
            let certs = self.certs.clone();
            let config = config.clone();
            tokio::spawn(async move {
                if let Err(e) = accept_tls(stream, peer, config, certs, routes, backends).await {
                    debug!(error = %e, "https connection closed");
                }
            });
        }
    }
}

async fn accept_tls(
    stream: TcpStream,
    peer: SocketAddr,
    config: Arc<ServerConfig>,
    certs: Arc<CertStore>,
    routes: Arc<RouteTable>,
    backends: Backends,
) -> Result<()> {
    let acceptor = LazyConfigAcceptor::new(Acceptor::default(), stream);
    let start = match acceptor.await {
        Ok(start) => start,
        Err(e) => {
            debug!(%peer, error = %e, "tls client hello rejected");
            return Ok(());
        }
    };

    if let Some(name) = start.client_hello().server_name() {
        let name = name.to_string();
        if let Err(e) = certs.resolve(&name).await {
            warn!(host = %name, error = %e, "failed to prepare certificate for tls handshake");
        }
    }

    let tls_stream = start.into_stream(config).await?;
    let io = TokioIo::new(tls_stream);
    let svc = service_fn(move |req| handle_https(req, routes.clone(), backends.clone(), peer));
    // Negotiated by ALPN: HTTP/1.1 keeps its upgrade support (websockets), h2
    // clients (gRPC) get streams and trailers.
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(H2_KEEP_ALIVE_INTERVAL)
        .keep_alive_timeout(H2_KEEP_ALIVE_TIMEOUT);
    if let Err(e) = builder.serve_connection_with_upgrades(io, svc).await {
        debug!(error = %e, "https connection error");
    }
    Ok(())
}

/// An h2 request carries its host in `:authority` (the URI), not a `Host`
/// header, but routing, the gates and the forwarded headers all read `Host`.
/// Synthesise it so every code path below sees the same thing for HTTP/1.1
/// and HTTP/2 clients. For HTTP/2 `:authority` always wins over a `Host`
/// header the client also sent (RFC 9113 §8.3.1), so a request cannot be
/// routed by one name and checked against another.
fn ensure_host_header(req: &mut Request<Incoming>) {
    if req.version() != Version::HTTP_2 && req.headers().contains_key(HOST) {
        return;
    }
    let Some(authority) = req.uri().authority().map(|a| a.as_str().to_string()) else {
        return;
    };
    if let Ok(v) = HeaderValue::from_str(&authority) {
        req.headers_mut().insert(HOST, v);
    }
}

/// Joins several `Cookie` headers into one with `; `. HTTP/2 clients send
/// one header per cookie (RFC 9113 §8.2.3), and an HTTP/1.1 backend, the
/// sticky-cookie lookup and the login gate all read only the first, so the
/// session cookie would be lost. Done once, before anything looks at the
/// request.
fn merge_cookie_headers(headers: &mut hyper::HeaderMap) {
    if headers.get_all(hyper::header::COOKIE).iter().count() < 2 {
        return;
    }
    let mut joined: Vec<u8> = Vec::new();
    for value in headers.get_all(hyper::header::COOKIE) {
        if !joined.is_empty() {
            joined.extend_from_slice(b"; ");
        }
        joined.extend_from_slice(value.as_bytes());
    }
    if let Ok(v) = HeaderValue::from_bytes(&joined) {
        headers.insert(hyper::header::COOKIE, v);
    }
}

fn route_for(routes: &RouteTable, req: &Request<Incoming>) -> Option<Arc<Route>> {
    let host = req.headers().get(HOST).and_then(|v| v.to_str().ok())?;
    routes.lookup(host, req.uri().path())
}

fn not_found(host: &str, path: &str) -> Response<ProxyBody> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(text_body(format!("no route for {host}{path}")))
        .unwrap_or_else(|_| Response::new(empty_body()))
}

fn bad_gateway(reason: impl std::fmt::Display) -> Response<ProxyBody> {
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .body(text_body(format!("bad gateway: {reason}")))
        .unwrap_or_else(|_| Response::new(empty_body()))
}

/// `e` followed by every error under it. The pooled client's own message is
/// only `client error (Connect)`; the why - connection refused, or which
/// node's relay could not reach the container - is in its sources.
fn with_causes(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

fn status_response(status: StatusCode, message: &str) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .body(text_body(message.to_string()))
        .unwrap_or_else(|_| Response::new(empty_body()))
}

/// Rejects a request whose TCP peer does not sit inside the route's
/// `allowed_subnet`, or one of its `trusted_overlay_subnets` (see that
/// field's doc for why a same-node client needs the latter). A route with no
/// restriction (a public network) always passes. This is a check on the real
/// socket peer, not a header, so it cannot be spoofed by a client claiming a
/// different address.
fn gate_by_network(route: &Route, peer: SocketAddr) -> Option<Response<ProxyBody>> {
    let subnet = route.options.allowed_subnet?;
    if subnet.contains(&peer.ip())
        || route
            .options
            .trusted_overlay_subnets
            .iter()
            .any(|s| s.contains(&peer.ip()))
    {
        return None;
    }
    Some(status_response(
        StatusCode::FORBIDDEN,
        "this route is only reachable from its own network",
    ))
}

/// Refuses a request whose caller's stack did not list the host on the
/// network the route is published on. The check is against the real socket
/// peer, like `gate_by_network`.
fn gate_by_allowed_hosts(
    peers: &PeerPolicies,
    route: &Route,
    peer: SocketAddr,
    host: &str,
) -> Option<Response<ProxyBody>> {
    let denied = peers.check(peer.ip(), route.options.network.as_deref(), host)?;
    debug!(
        stack = %denied.stack, network = %denied.network, host = %denied.host,
        %peer, "host not in the stack's allowed list",
    );
    Some(status_response(
        StatusCode::FORBIDDEN,
        &format!(
            "stack {} may not reach {} through network {}: it is not in the network's allowed list",
            denied.stack, denied.host, denied.network
        ),
    ))
}

/// Answers a redirect route directly, never touching a backend.
fn respond_redirect(redirect: &RedirectTarget, path_and_query: &str) -> Response<ProxyBody> {
    let location = if redirect.preserve_path {
        format!("{}{path_and_query}", redirect.url)
    } else {
        redirect.url.clone()
    };
    let status = StatusCode::from_u16(redirect.status).unwrap_or(StatusCode::FOUND);
    Response::builder()
        .status(status)
        .header(hyper::header::LOCATION, location)
        .body(empty_body())
        .unwrap_or_else(|_| Response::new(empty_body()))
}

/// Reject an oversized body before reading it, when the client declared one.
///
/// This is the cheap half of the limit: a `Content-Length` over the cap is
/// refused without touching the body at all, so an upload that was never going
/// to be accepted does not get streamed across the network first. A chunked
/// request has no length to check, and is caught by `Limited` while streaming.
fn declared_length_exceeds(headers: &hyper::HeaderMap, max: u64) -> bool {
    headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|len| len > max)
}

/// Apply the route's body cap, if it has one. `Limited` fails the body mid
/// stream once the cap is passed, which surfaces to the backend as a broken
/// request rather than a truncated one.
fn limited_body(body: Incoming, max: Option<u64>) -> ProxyBody {
    match max {
        Some(max) => Limited::new(incoming_to_proxy_body(body), max as usize)
            .map_err(|e| Box::new(std::io::Error::other(e.to_string())) as BoxError)
            .boxed(),
        None => incoming_to_proxy_body(body),
    }
}

/// The full URL a redirect (fixed-string or regex) needs to match/build
/// against: `{scheme}://{host}{path_and_query}`, mirroring `respond_redirect`.
fn full_url(scheme: &str, host: &str, path_and_query: &str) -> String {
    format!("{scheme}://{host}{path_and_query}")
}

/// Answers with a `redirect_regex` match, if the route carries one and it
/// matches this request's full URL. `None` means fall through to normal
/// proxying (or the plain-http-to-https redirect, whichever comes next).
fn respond_redirect_regex(
    route: &Route,
    scheme: &str,
    host: &str,
    path_and_query: &str,
) -> Option<Response<ProxyBody>> {
    let (regex, cfg) = route.redirect_regex()?;
    let url = full_url(scheme, host, path_and_query);
    if !regex.is_match(&url) {
        return None;
    }
    let location = regex.replace(&url, cfg.replacement.as_str()).into_owned();
    let status = match (cfg.permanent, scheme) {
        (true, _) => StatusCode::PERMANENT_REDIRECT,
        (false, _) => StatusCode::TEMPORARY_REDIRECT,
    };
    Some(
        Response::builder()
            .status(status)
            .header(hyper::header::LOCATION, location)
            .body(empty_body())
            .unwrap_or_else(|_| Response::new(empty_body())),
    )
}

async fn handle_http(
    mut req: Request<Incoming>,
    routes: Arc<RouteTable>,
    backends: Backends,
    peer: SocketAddr,
) -> std::result::Result<Response<ProxyBody>, Infallible> {
    merge_cookie_headers(req.headers_mut());
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let path = req.uri().path().to_string();

    if let Some(token) = path.strip_prefix(ACME_CHALLENGE_PREFIX) {
        return Ok(http01_response(&backends.http01, token));
    }

    if let Some(gate) = backends
        .auth_gate
        .as_ref()
        .filter(|g| g.is_portal_host(&host))
    {
        return Ok(gate.handle_portal(&req, &routes, "http", peer.ip()).await);
    }

    let Some(route) = route_for(&routes, &req) else {
        return Ok(not_found(&host, &path));
    };

    if let Some(resp) = gate_by_network(&route, peer) {
        return Ok(resp);
    }

    if let Some(resp) = gate_by_allowed_hosts(&backends.peers, &route, peer, &host) {
        return Ok(resp);
    }

    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");

    if let Some(redirect) = &route.redirect {
        return Ok(respond_redirect(redirect, path_and_query));
    }

    if let Some(resp) = respond_redirect_regex(&route, "http", &host, path_and_query) {
        return Ok(resp);
    }

    if route.tls {
        let resp = middleware::redirect_https(&host, path_and_query)
            .body(empty_body())
            .unwrap_or_else(|_| Response::new(empty_body()));
        return Ok(resp);
    }

    Ok(proxy(req, &route, "http", peer, &backends).await)
}

/// Answers an ACME HTTP-01 validation request straight out of the shared
/// store, ahead of any routing decision - the CA's validator sends no `Host`
/// clouduler would recognize, and the hostname being validated may have no
/// route at all yet (issuance for a brand new name).
fn http01_response(store: &Http01Store, token: &str) -> Response<ProxyBody> {
    match store.get(token) {
        Some(key_authorization) => Response::builder()
            .status(StatusCode::OK)
            .header(hyper::header::CONTENT_TYPE, "text/plain")
            .body(text_body(key_authorization))
            .unwrap_or_else(|_| Response::new(empty_body())),
        None => status_response(StatusCode::NOT_FOUND, "no acme challenge for this token"),
    }
}

async fn handle_https(
    mut req: Request<Incoming>,
    routes: Arc<RouteTable>,
    backends: Backends,
    peer: SocketAddr,
) -> std::result::Result<Response<ProxyBody>, Infallible> {
    ensure_host_header(&mut req);
    merge_cookie_headers(req.headers_mut());
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let path = req.uri().path().to_string();

    if let Some(gate) = backends
        .auth_gate
        .as_ref()
        .filter(|g| g.is_portal_host(&host))
    {
        return Ok(gate.handle_portal(&req, &routes, "https", peer.ip()).await);
    }

    let Some(route) = route_for(&routes, &req) else {
        return Ok(not_found(&host, &path));
    };

    if let Some(resp) = gate_by_network(&route, peer) {
        return Ok(resp);
    }

    if let Some(resp) = gate_by_allowed_hosts(&backends.peers, &route, peer, &host) {
        return Ok(resp);
    }

    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");

    if let Some(redirect) = &route.redirect {
        return Ok(respond_redirect(redirect, path_and_query));
    }

    if let Some(resp) = respond_redirect_regex(&route, "https", &host, path_and_query) {
        return Ok(resp);
    }

    Ok(proxy(req, &route, "https", peer, &backends).await)
}

/// Applies `strip_prefix` to the request path.
fn apply_route_middlewares(route: &Route, req: &mut Request<Incoming>) {
    if route
        .middlewares
        .contains(&cl_config::Middleware::StripPrefix)
    {
        if let Some(prefix) = &route.path_prefix {
            let new_path = middleware::strip_prefix(req.uri().path(), prefix);
            let path_and_query = match req.uri().query() {
                Some(q) => format!("{new_path}?{q}"),
                None => new_path,
            };
            if let Ok(pq) = path_and_query.parse::<http::uri::PathAndQuery>() {
                let mut parts = req.uri().clone().into_parts();
                parts.path_and_query = Some(pq);
                if let Ok(uri) = http::Uri::from_parts(parts) {
                    *req.uri_mut() = uri;
                }
            }
        }
    }
}

/// The route's `headers` middleware, if it has one.
fn headers_config(route: &Route) -> Option<&cl_config::Headers> {
    route.middlewares.iter().find_map(|m| match m {
        cl_config::Middleware::Headers(cfg) => Some(cfg),
        _ => None,
    })
}

/// The route's `cors` middleware, if it has one.
fn cors_policy(route: &Route) -> Option<&cl_config::CorsPolicy> {
    route.middlewares.iter().find_map(|m| match m {
        cl_config::Middleware::Cors(policy) => Some(policy),
        _ => None,
    })
}

/// The route's `authenticated` middleware, if it has one.
fn authenticated_policy(route: &Route) -> Option<&cl_config::Authenticated> {
    route.middlewares.iter().find_map(|m| match m {
        cl_config::Middleware::Authenticated(policy) => Some(policy),
        _ => None,
    })
}

async fn proxy(
    mut req: Request<Incoming>,
    route: &Route,
    scheme: &str,
    peer: SocketAddr,
    backends: &Backends,
) -> Response<ProxyBody> {
    apply_route_middlewares(route, &mut req);

    if let Some(policy) = authenticated_policy(route) {
        let Some(gate) = &backends.auth_gate else {
            return status_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "this route requires login, but the proxy has no auth gate configured",
            );
        };
        let internal = backends.peers.is_internal(peer.ip());
        match gate
            .evaluate(
                &req,
                scheme,
                policy,
                route.options.network.as_deref(),
                internal,
                peer.ip(),
            )
            .await
        {
            GateOutcome::Allow => {
                // The backend has no business holding a replayable session.
                crate::auth_gate::strip_gate_cookies(req.headers_mut());
            }
            GateOutcome::Respond(resp) => return resp,
        }
    }

    if let Some(cfg) = headers_config(route) {
        middleware::apply_headers(req.headers_mut(), &cfg.request, &cfg.remove_request);
    }

    let cors = cors_policy(route);
    let origin = req
        .headers()
        .get(hyper::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    if let Some(policy) = cors {
        if middleware::is_cors_preflight(req.method(), req.headers()) {
            let requested = req
                .headers()
                .get("access-control-request-headers")
                .and_then(|v| v.to_str().ok());
            let mut resp = Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(empty_body())
                .unwrap_or_else(|_| Response::new(empty_body()));
            if let Some(origin) = &origin {
                for (name, value) in middleware::cors_response_headers(policy, origin, requested) {
                    resp.headers_mut().insert(name, value);
                }
            }
            return resp;
        }
    }

    let sticky_cookie = extract_cookie(req.headers(), STICKY_COOKIE);
    let Some(picked) = route.pick(sticky_cookie.as_deref()) else {
        return bad_gateway("no backend available for route");
    };

    let options = &route.options;

    if let Some(max) = options.max_body_size {
        if declared_length_exceeds(req.headers(), max) {
            return status_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("request body exceeds the {max} byte limit for this route"),
            );
        }
    }

    if middleware::is_upgrade(req.headers()) {
        if !options.websockets {
            return status_response(
                StatusCode::BAD_REQUEST,
                "connection upgrades are not enabled for this route",
            );
        }
        return match proxy_websocket(req, picked.addr, scheme, peer, &backends.dialer).await {
            Ok(resp) => resp,
            Err(e) => bad_gateway(e),
        };
    }

    let forwarded_host = req.headers().get(HOST).cloned();
    let (mut parts, body) = req.into_parts();
    // gRPC servers insist on `te: trailers`, which is hop-by-hop and so
    // stripped below; it is the one value HTTP/2 allows, so put it back.
    let te_trailers = options.http2 && wants_trailers(&parts.headers);
    middleware::strip_hop_by_hop(&mut parts.headers);
    if te_trailers {
        parts
            .headers
            .insert(hyper::header::TE, HeaderValue::from_static("trailers"));
    }
    add_forwarded_headers(&mut parts.headers, scheme, peer, forwarded_host.as_ref());
    // The inbound request may be HTTP/2; the version has to match the client
    // it goes out on, whatever the visitor spoke.
    parts.version = if options.http2 {
        Version::HTTP_2
    } else {
        Version::HTTP_11
    };

    let Ok(uri) = backend_uri(picked.addr, &parts.uri) else {
        return bad_gateway("could not build backend uri");
    };
    parts.uri = uri;
    // Rewriting Host to the backend address is the default because most
    // containers ignore it; preserve_host leaves the client's value alone for
    // the ones that build absolute URLs or virtual host on it.
    if !options.preserve_host {
        if let Ok(host) = HeaderValue::from_str(&picked.addr.to_string()) {
            parts.headers.insert(HOST, host);
        }
    }

    let outbound = Request::from_parts(parts, limited_body(body, options.max_body_size));

    let client = if options.http2 {
        &backends.client_h2
    } else {
        &backends.client
    };
    let sent = client.request(outbound);
    let result = match response_limit(options) {
        Some(limit) => match tokio::time::timeout(limit, sent).await {
            Ok(result) => result,
            Err(_) => {
                return status_response(
                    StatusCode::GATEWAY_TIMEOUT,
                    &format!("{} did not respond within {limit:?}", picked.addr),
                )
            }
        },
        None => sent.await,
    };

    let mut resp = match result {
        Ok(resp) => {
            let (rparts, rbody) = resp.into_parts();
            let mut rparts = rparts;
            middleware::strip_hop_by_hop(&mut rparts.headers);
            // An h2c backend answers as HTTP/2; the visitor's connection
            // (HTTP/1.1 or h2) encodes its own version.
            rparts.version = Version::HTTP_11;
            Response::from_parts(rparts, incoming_to_proxy_body(rbody))
        }
        Err(e) => return bad_gateway(format!("{} unreachable: {}", picked.addr, with_causes(&e))),
    };

    if let Some(cfg) = headers_config(route) {
        middleware::apply_headers(resp.headers_mut(), &cfg.response, &cfg.remove_response);
    }
    if let Some(value) = picked.set_cookie {
        if let Ok(v) = HeaderValue::from_str(&format!("{STICKY_COOKIE}={value}; Path=/; HttpOnly"))
        {
            resp.headers_mut().insert(hyper::header::SET_COOKIE, v);
        }
    }
    if let (Some(policy), Some(origin)) = (cors, &origin) {
        for (name, value) in middleware::cors_response_headers(policy, origin, None) {
            resp.headers_mut().insert(name, value);
        }
    }
    resp
}

/// How long the backend has to return response headers, or `None` for no
/// limit. Unset is 30s on a plain route and unlimited on an `http2` one (a
/// gRPC stream may legitimately send nothing for a long time, and gRPC
/// clients set their own deadlines); an explicit `0s` removes the limit on
/// any route. The limit only ever covers the headers: bodies and trailers
/// stream with no timer at all.
fn response_limit(options: &crate::route::RouteOptions) -> Option<Duration> {
    match options.response_timeout {
        Some(limit) if limit.is_zero() => None,
        Some(limit) => Some(limit),
        None if options.http2 => None,
        None => Some(DEFAULT_RESPONSE_TIMEOUT),
    }
}

/// Whether the client's `TE` header lists `trailers`.
fn wants_trailers(headers: &hyper::HeaderMap) -> bool {
    headers
        .get_all(hyper::header::TE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| {
            v.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("trailers"))
        })
}

fn backend_uri(
    addr: SocketAddr,
    original: &http::Uri,
) -> std::result::Result<http::Uri, http::Error> {
    let path_and_query = original.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    format!("http://{addr}{path_and_query}")
        .parse::<http::Uri>()
        .map_err(Into::into)
}

fn extract_cookie(headers: &hyper::HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(hyper::header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|kv| {
        let kv = kv.trim();
        let (k, v) = kv.split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

/// Adds `X-Forwarded-For` (appending to any existing chain), `X-Forwarded-Proto`
/// and `X-Forwarded-Host`.
fn add_forwarded_headers(
    headers: &mut hyper::HeaderMap,
    scheme: &str,
    peer: SocketAddr,
    forwarded_host: Option<&HeaderValue>,
) {
    let client_ip = peer.ip().to_string();
    let xff = match headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        Some(existing) => format!("{existing}, {client_ip}"),
        None => client_ip,
    };
    if let Ok(v) = HeaderValue::from_str(&xff) {
        headers.insert("x-forwarded-for", v);
    }
    if let Ok(v) = HeaderValue::from_str(scheme) {
        headers.insert("x-forwarded-proto", v);
    }
    if let Some(host) = forwarded_host {
        headers.insert("x-forwarded-host", host.clone());
    }
}

/// Proxies a websocket (or other `Upgrade`) request: sends the request to the
/// backend over its own connection, and once both sides have upgraded,
/// splices the two raw connections together.
async fn proxy_websocket(
    mut req: Request<Incoming>,
    backend: SocketAddr,
    scheme: &str,
    peer: SocketAddr,
    dialer: &Dialer,
) -> Result<Response<ProxyBody>> {
    let client_upgrade = hyper::upgrade::on(&mut req);

    let forwarded_host = req.headers().get(HOST).cloned();
    let (mut parts, _body) = req.into_parts();
    add_forwarded_headers(&mut parts.headers, scheme, peer, forwarded_host.as_ref());
    if let Ok(host) = HeaderValue::from_str(&backend.to_string()) {
        parts.headers.insert(HOST, host);
    }
    // Unlike the regular path (which goes through hyper_util's pooled Client and
    // gets its absolute-form URI normalized to origin-form automatically), this
    // sends over a raw hyper::client::conn connection that writes parts.uri to
    // the wire verbatim. An absolute-form request-target (http://addr/path)
    // reaches the backend as `GET http://addr/path HTTP/1.1`, which Node's
    // upgrade handler passes through as req.url unchanged — engine.io/socket.io
    // then fails to match it against the expected path and silently drops the
    // connection. Origin-form (path+query only) is what a reverse proxy must
    // send to an origin server.
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    parts.uri = path_and_query
        .parse()
        .map_err(|e: http::uri::InvalidUri| crate::error::ProxyError::Tls(e.to_string()))?;

    let backend_req = Request::from_parts(parts, Empty::<Bytes>::new());

    let stream = dialer
        .connect_tcp(backend, BACKEND_CONNECT_TIMEOUT)
        .await
        .map_err(|e| crate::error::ProxyError::BackendUnreachable {
            addr: backend,
            source: e,
        })?;
    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| crate::error::ProxyError::BackendUnreachable {
            addr: backend,
            source: std::io::Error::other(e.to_string()),
        })?;
    tokio::spawn(async move {
        if let Err(e) = conn.with_upgrades().await {
            debug!(error = %e, "backend websocket connection closed");
        }
    });

    let mut backend_resp = sender.send_request(backend_req).await.map_err(|e| {
        crate::error::ProxyError::BackendUnreachable {
            addr: backend,
            source: std::io::Error::other(e.to_string()),
        }
    })?;

    if backend_resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        let (rparts, rbody) = backend_resp.into_parts();
        return Ok(Response::from_parts(rparts, incoming_to_proxy_body(rbody)));
    }

    let backend_upgrade = hyper::upgrade::on(&mut backend_resp);
    let (rparts, _) = backend_resp.into_parts();
    let mut response = Response::new(empty_body());
    *response.status_mut() = rparts.status;
    *response.headers_mut() = rparts.headers;

    tokio::spawn(async move {
        let (client_io, backend_io) = match (client_upgrade.await, backend_upgrade.await) {
            (Ok(c), Ok(b)) => (c, b),
            _ => {
                debug!("websocket upgrade did not complete on both sides");
                return;
            }
        };
        let mut client_io = TokioIo::new(client_io);
        let mut backend_io = TokioIo::new(backend_io);
        if let Err(e) = tokio::io::copy_bidirectional(&mut client_io, &mut backend_io).await {
            debug!(error = %e, "websocket relay ended");
        }
    });

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::RouteOptions;

    fn limit(response_timeout: Option<Duration>, http2: bool) -> Option<Duration> {
        response_limit(&RouteOptions {
            response_timeout,
            http2,
            ..RouteOptions::default()
        })
    }

    #[test]
    fn response_limit_defaults_per_protocol_and_zero_disables_it() {
        assert_eq!(limit(None, false), Some(DEFAULT_RESPONSE_TIMEOUT));
        assert_eq!(limit(None, true), None, "a gRPC stream has no default cap");
        assert_eq!(limit(Some(Duration::ZERO), false), None);
        assert_eq!(limit(Some(Duration::ZERO), true), None);
        let five = Some(Duration::from_secs(5));
        assert_eq!(limit(five, false), five);
        assert_eq!(limit(five, true), five);
    }

    #[test]
    fn split_cookie_headers_are_joined_into_one() {
        let mut h = hyper::HeaderMap::new();
        merge_cookie_headers(&mut h);
        assert!(h.get("cookie").is_none());
        h.append("cookie", "a=1".parse().unwrap());
        merge_cookie_headers(&mut h);
        assert_eq!(h.get_all("cookie").iter().count(), 1);
        h.append("cookie", "authentik_session=s; b=2".parse().unwrap());
        h.append("cookie", "c=3".parse().unwrap());
        merge_cookie_headers(&mut h);
        let all: Vec<_> = h.get_all("cookie").iter().collect();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0], "a=1; authentik_session=s; b=2; c=3");
    }

    #[test]
    fn te_trailers_is_detected_among_other_tokens() {
        let mut h = hyper::HeaderMap::new();
        assert!(!wants_trailers(&h));
        h.insert("te", "gzip, Trailers".parse().unwrap());
        assert!(wants_trailers(&h));
    }
}
