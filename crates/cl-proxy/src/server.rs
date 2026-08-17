//! The hyper 1.x listeners: plain HTTP (redirect or proxy) and HTTPS
//! (TLS terminated here, then proxied).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderValue, HOST};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::server::Acceptor;
use rustls::ServerConfig;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::LazyConfigAcceptor;
use tracing::{debug, warn};

use crate::certs::{ensure_crypto_provider, CertResolver, CertStore};
use crate::error::Result;
use crate::middleware;
use crate::route::{Route, RouteTable, STICKY_COOKIE};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
/// The single body type every response and outbound request in this crate
/// uses, so both listeners and the backend client agree on one shape.
pub type ProxyBody = BoxBody<Bytes, BoxError>;

fn empty_body() -> ProxyBody {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed()
}

fn text_body(s: impl Into<Bytes>) -> ProxyBody {
    Full::new(s.into()).map_err(|never| match never {}).boxed()
}

fn incoming_to_proxy_body(b: Incoming) -> ProxyBody {
    b.map_err(|e| Box::new(e) as BoxError).boxed()
}

/// Runs the plain HTTP and TLS listeners for one routing table and cert store.
pub struct ProxyServer {
    routes: Arc<RouteTable>,
    certs: Arc<CertStore>,
    client: Client<HttpConnector, ProxyBody>,
}

impl ProxyServer {
    pub fn new(routes: Arc<RouteTable>, certs: Arc<CertStore>) -> Self {
        let client = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
        Self {
            routes,
            certs,
            client,
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
            let client = self.client.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let svc = service_fn(move |req| {
                    handle_http(req, routes.clone(), client.clone(), peer)
                });
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
            let client = self.client.clone();
            let certs = self.certs.clone();
            let config = config.clone();
            tokio::spawn(async move {
                if let Err(e) = accept_tls(stream, peer, config, certs, routes, client).await {
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
    client: Client<HttpConnector, ProxyBody>,
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
    let svc = service_fn(move |req| handle_https(req, routes.clone(), client.clone(), peer));
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

async fn handle_http(
    req: Request<Incoming>,
    routes: Arc<RouteTable>,
    client: Client<HttpConnector, ProxyBody>,
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

    if route.tls {
        let path_and_query = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/");
        let resp = middleware::redirect_https(&host, path_and_query)
            .body(empty_body())
            .unwrap_or_else(|_| Response::new(empty_body()));
        return Ok(resp);
    }

    Ok(proxy(req, &route, "http", peer, &client).await)
}

async fn handle_https(
    req: Request<Incoming>,
    routes: Arc<RouteTable>,
    client: Client<HttpConnector, ProxyBody>,
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

    Ok(proxy(req, &route, "https", peer, &client).await)
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

async fn proxy(
    mut req: Request<Incoming>,
    route: &Route,
    scheme: &str,
    peer: SocketAddr,
    client: &Client<HttpConnector, ProxyBody>,
) -> Response<ProxyBody> {
    apply_route_middlewares(route, &mut req);

    let sticky_cookie = extract_cookie(req.headers(), STICKY_COOKIE);
    let Some(picked) = route.pick(sticky_cookie.as_deref()) else {
        return bad_gateway("no backend available for route");
    };

    if middleware::is_upgrade(req.headers()) {
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
    if let Ok(host) = HeaderValue::from_str(&picked.addr.to_string()) {
        parts.headers.insert(HOST, host);
    }

    let outbound = Request::from_parts(parts, incoming_to_proxy_body(body));

    let mut resp = match client.request(outbound).await {
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
    let uri = backend_uri(backend, &parts.uri).map_err(|e| crate::error::ProxyError::Tls(e.to_string()))?;
    parts.uri = uri;

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
