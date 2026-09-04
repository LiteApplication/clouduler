//! The hyper 1.x listeners: plain HTTP (redirect or proxy) and HTTPS
//! (TLS terminated here, then proxied).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderValue, HOST};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use rustls::server::Acceptor;
use rustls::ServerConfig;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::LazyConfigAcceptor;
use tracing::{debug, warn};

use crate::auth_gate::{AuthGate, GateOutcome};
use crate::certs::{ensure_crypto_provider, CertResolver, CertStore};
use crate::error::Result;
use crate::forward_auth;
use crate::middleware;
use crate::route::{RedirectTarget, Route, RouteTable, STICKY_COOKIE};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
/// The single body type every response and outbound request in this crate
/// uses, so both listeners and the backend client agree on one shape.
pub type ProxyBody = BoxBody<Bytes, BoxError>;

pub(crate) fn empty_body() -> ProxyBody {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed()
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
    client: Client<HttpConnector, ProxyBody>,
    forward_auth_client: reqwest::Client,
    auth_gate: Option<Arc<AuthGate>>,
}

/// Runs the plain HTTP and TLS listeners for one routing table and cert store.
pub struct ProxyServer {
    routes: Arc<RouteTable>,
    certs: Arc<CertStore>,
    client: Client<HttpConnector, ProxyBody>,
    /// Separate from `client`: a `forward_auth` endpoint can be external and
    /// speak TLS, unlike backends, which are always plain HTTP over the
    /// overlay and reachable through the internal `HttpConnector`.
    forward_auth_client: reqwest::Client,
    /// `None` when nothing needs it yet: only built once a route actually
    /// carries the `authenticated` middleware.
    auth_gate: Option<Arc<AuthGate>>,
}

/// How long a backend gets to accept a TCP connection before it's treated as
/// unreachable. Without this, `HttpConnector` falls back to the OS's own SYN
/// retry timeout (minutes, not seconds) on a backend that never sends a
/// SYN-ACK or RST - a route on a node other than the one running its
/// container relies on this to fail fast instead of hanging the request.
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

impl ProxyServer {
    pub fn new(routes: Arc<RouteTable>, certs: Arc<CertStore>) -> Self {
        Self::with_auth_gate(routes, certs, None)
    }

    pub fn with_auth_gate(routes: Arc<RouteTable>, certs: Arc<CertStore>, auth_gate: Option<Arc<AuthGate>>) -> Self {
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(BACKEND_CONNECT_TIMEOUT));
        let client = Client::builder(TokioExecutor::new())
            .pool_timer(TokioTimer::new())
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .build(connector);
        Self {
            routes,
            certs,
            client,
            forward_auth_client: reqwest::Client::new(),
            auth_gate,
        }
    }

    /// Serves plain HTTP forever: 308-redirects tls routes to https, proxies
    /// everything else directly.
    pub async fn serve_http(&self, addr: SocketAddr) -> Result<()> {
        let listener = TcpListener::bind(addr).await?;
        debug!(%addr, "http listener started");
        loop {
            let (stream, peer) = listener.accept().await?;
            let routes = self.routes.clone();
            let backends = Backends {
                client: self.client.clone(),
                forward_auth_client: self.forward_auth_client.clone(),
                auth_gate: self.auth_gate.clone(),
            };
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let svc = service_fn(move |req| handle_http(req, routes.clone(), backends.clone(), peer));
                if let Err(e) = http1::Builder::new().serve_connection(io, svc).with_upgrades().await {
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
        ensure_crypto_provider();
        let resolver = Arc::new(CertResolver::new(self.certs.clone()));
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(resolver);
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);

        let listener = TcpListener::bind(addr).await?;
        debug!(%addr, "https listener started");
        loop {
            let (stream, peer) = listener.accept().await?;
            let routes = self.routes.clone();
            let backends = Backends {
                client: self.client.clone(),
                forward_auth_client: self.forward_auth_client.clone(),
                auth_gate: self.auth_gate.clone(),
            };
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
    if let Err(e) = http1::Builder::new().serve_connection(io, svc).with_upgrades().await {
        debug!(error = %e, "https connection error");
    }
    Ok(())
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

fn status_response(status: StatusCode, message: &str) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .body(text_body(message.to_string()))
        .unwrap_or_else(|_| Response::new(empty_body()))
}

/// Rejects a request whose TCP peer does not sit inside the route's
/// `allowed_subnet`. A route with no restriction (a public network) always
/// passes. This is a check on the real socket peer, not a header, so it
/// cannot be spoofed by a client claiming a different address.
fn gate_by_network(route: &Route, peer: SocketAddr) -> Option<Response<ProxyBody>> {
    let subnet = route.options.allowed_subnet?;
    if subnet.contains(&peer.ip()) {
        return None;
    }
    Some(status_response(
        StatusCode::FORBIDDEN,
        "this route is only reachable from its own network",
    ))
}

/// Answers a redirect route directly, never touching a backend.
fn respond_redirect(
    redirect: &RedirectTarget,
    path_and_query: &str,
) -> Response<ProxyBody> {
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
fn respond_redirect_regex(route: &Route, scheme: &str, host: &str, path_and_query: &str) -> Option<Response<ProxyBody>> {
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
    req: Request<Incoming>,
    routes: Arc<RouteTable>,
    backends: Backends,
    peer: SocketAddr,
) -> std::result::Result<Response<ProxyBody>, Infallible> {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let path = req.uri().path().to_string();

    let Some(route) = route_for(&routes, &req) else {
        return Ok(not_found(&host, &path));
    };

    if let Some(resp) = gate_by_network(&route, peer) {
        return Ok(resp);
    }

    let path_and_query = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");

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

async fn handle_https(
    req: Request<Incoming>,
    routes: Arc<RouteTable>,
    backends: Backends,
    peer: SocketAddr,
) -> std::result::Result<Response<ProxyBody>, Infallible> {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let path = req.uri().path().to_string();

    let Some(route) = route_for(&routes, &req) else {
        return Ok(not_found(&host, &path));
    };

    if let Some(resp) = gate_by_network(&route, peer) {
        return Ok(resp);
    }

    let path_and_query = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");

    if let Some(redirect) = &route.redirect {
        return Ok(respond_redirect(redirect, path_and_query));
    }

    if let Some(resp) = respond_redirect_regex(&route, "https", &host, path_and_query) {
        return Ok(resp);
    }

    Ok(proxy(req, &route, "https", peer, &backends).await)
}

/// Applies `strip_prefix` and `basic_auth` in that order, ahead of `headers`.
/// `basic_auth` credentials and `headers` values aren't modeled in
/// `cl-config` yet (`Middleware` there is a bare tag with no per-route
/// config), so both are currently no-ops here; the pure functions they'd call
/// live in `middleware.rs` and are unit tested there.
fn apply_route_middlewares(route: &Route, req: &mut Request<Incoming>) {
    if route.middlewares.contains(&cl_config::Middleware::StripPrefix) {
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

/// The route's `cors` middleware, if it has one.
fn cors_policy(route: &Route) -> Option<&cl_config::CorsPolicy> {
    route.middlewares.iter().find_map(|m| match m {
        cl_config::Middleware::Cors(policy) => Some(policy),
        _ => None,
    })
}

/// The route's `forward_auth` middleware, if it has one.
fn forward_auth_config(route: &Route) -> Option<&cl_config::ForwardAuth> {
    route.middlewares.iter().find_map(|m| match m {
        cl_config::Middleware::ForwardAuth(cfg) => Some(cfg),
        _ => None,
    })
}

/// Whether the route carries the `authenticated` middleware.
fn is_authenticated_route(route: &Route) -> bool {
    route.middlewares.contains(&cl_config::Middleware::Authenticated)
}

/// Turns a `forward_auth::Outcome::Deny` into the response sent to the
/// client, relaying the auth endpoint's own status/headers/body verbatim.
fn deny_response(status: StatusCode, headers: hyper::HeaderMap, body: Bytes) -> Response<ProxyBody> {
    let mut builder = Response::builder().status(status);
    if let Some(map) = builder.headers_mut() {
        *map = headers;
    }
    builder
        .body(Full::new(body).map_err(|never| match never {}).boxed())
        .unwrap_or_else(|_| Response::new(empty_body()))
}

async fn proxy(mut req: Request<Incoming>, route: &Route, scheme: &str, peer: SocketAddr, backends: &Backends) -> Response<ProxyBody> {
    apply_route_middlewares(route, &mut req);

    if is_authenticated_route(route) {
        let Some(gate) = &backends.auth_gate else {
            return status_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "this route requires login, but the proxy has no auth gate configured",
            );
        };
        match gate.evaluate(&req, scheme).await {
            GateOutcome::Allow => {}
            GateOutcome::Respond(resp) => return resp,
        }
    }

    if let Some(cfg) = forward_auth_config(route) {
        match forward_auth::check(&backends.forward_auth_client, cfg, req.method(), req.uri(), req.headers()).await {
            forward_auth::Outcome::Deny(status, headers, body) => return deny_response(status, headers, body),
            forward_auth::Outcome::Allow(allowed) => {
                for (name, value) in allowed {
                    req.headers_mut().insert(name, value);
                }
            }
        }
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
        return match proxy_websocket(req, picked.addr, scheme, peer).await {
            Ok(resp) => resp,
            Err(e) => bad_gateway(e),
        };
    }

    let forwarded_host = req.headers().get(HOST).cloned();
    let (mut parts, body) = req.into_parts();
    middleware::strip_hop_by_hop(&mut parts.headers);
    add_forwarded_headers(&mut parts.headers, scheme, peer, forwarded_host.as_ref());

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

    let sent = backends.client.request(outbound);
    let limit = options.response_timeout.unwrap_or(DEFAULT_RESPONSE_TIMEOUT);
    let result = match tokio::time::timeout(limit, sent).await {
        Ok(result) => result,
        Err(_) => {
            return status_response(
                StatusCode::GATEWAY_TIMEOUT,
                &format!("{} did not respond within {limit:?}", picked.addr),
            )
        }
    };

    let mut resp = match result {
        Ok(resp) => {
            let (rparts, rbody) = resp.into_parts();
            let mut rparts = rparts;
            middleware::strip_hop_by_hop(&mut rparts.headers);
            Response::from_parts(rparts, incoming_to_proxy_body(rbody))
        }
        Err(e) => return bad_gateway(format!("{} unreachable: {e}", picked.addr)),
    };

    if let Some(value) = picked.set_cookie {
        if let Ok(v) = HeaderValue::from_str(&format!("{STICKY_COOKIE}={value}; Path=/; HttpOnly")) {
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

fn backend_uri(addr: SocketAddr, original: &http::Uri) -> std::result::Result<http::Uri, http::Error> {
    let path_and_query = original.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    format!("http://{addr}{path_and_query}").parse::<http::Uri>().map_err(Into::into)
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
    let path_and_query =
        parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/").to_string();
    parts.uri = path_and_query.parse().map_err(|e: http::uri::InvalidUri| {
        crate::error::ProxyError::Tls(e.to_string())
    })?;

    let backend_req = Request::from_parts(parts, Empty::<Bytes>::new());

    let stream = TcpStream::connect(backend)
        .await
        .map_err(|e| crate::error::ProxyError::BackendUnreachable { addr: backend, source: e })?;
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

    let mut backend_resp = sender
        .send_request(backend_req)
        .await
        .map_err(|e| crate::error::ProxyError::BackendUnreachable {
            addr: backend,
            source: std::io::Error::other(e.to_string()),
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
