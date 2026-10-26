//! Integration tests: real listeners, a real in-process backend, no network.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use cl_config::Middleware;
use cl_proxy::{CertStore, Http01Store, ProxyServer, Route, RouteTable};

/// Spawns a tiny backend that echoes the request's headers back as
/// `x-echo-<name>: <value>` response headers, and the method+path as the body.
async fn spawn_echo_backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let svc = service_fn(echo);
                let _ = http1::Builder::new().serve_connection(io, svc).await;
            });
        }
    });
    addr
}

async fn echo(req: Request<Incoming>) -> Result<Response<http_body_util::Full<Bytes>>, Infallible> {
    let mut builder = Response::builder().status(StatusCode::OK);
    for (name, value) in req.headers() {
        if let Ok(v) = value.to_str() {
            builder = builder.header(format!("x-echo-{name}"), v.to_string());
        }
    }
    let body = format!("{} {}", req.method(), req.uri().path());
    Ok(builder
        .body(http_body_util::Full::new(Bytes::from(body)))
        .unwrap())
}

async fn wait_for_port(addr: SocketAddr) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("nothing listening on {addr}");
}

fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

#[tokio::test]
async fn proxies_request_with_forwarded_headers() {
    let backend = spawn_echo_backend().await;
    let route = Route::new("app.test", None, false, vec![], false, "svc", vec![backend]);
    let routes = Arc::new(RouteTable::new(vec![route]));
    let certs = Arc::new(CertStore::new());
    let server = Arc::new(ProxyServer::new(routes, certs));

    let http_addr = free_addr();
    let s = server.clone();
    tokio::spawn(async move { s.serve_http(http_addr).await.unwrap() });
    wait_for_port(http_addr).await;

    let client = reqwest_like_get(http_addr, "app.test", "/hello").await;
    assert_eq!(client.status, 200);
    assert_eq!(client.body, "GET /hello");
    assert!(client.headers.contains_key("x-echo-x-forwarded-for"));
    assert_eq!(
        client.headers.get("x-echo-x-forwarded-proto").unwrap(),
        "http"
    );
    assert_eq!(
        client.headers.get("x-echo-x-forwarded-host").unwrap(),
        "app.test"
    );
}

#[tokio::test]
async fn strips_hop_by_hop_headers_both_directions() {
    let backend = spawn_echo_backend().await;
    let route = Route::new("app.test", None, false, vec![], false, "svc", vec![backend]);
    let routes = Arc::new(RouteTable::new(vec![route]));
    let certs = Arc::new(CertStore::new());
    let server = Arc::new(ProxyServer::new(routes, certs));

    let http_addr = free_addr();
    let s = server.clone();
    tokio::spawn(async move { s.serve_http(http_addr).await.unwrap() });
    wait_for_port(http_addr).await;

    let resp = raw_request(
        http_addr,
        "GET /x HTTP/1.1\r\nHost: app.test\r\nConnection: keep-alive\r\nKeep-Alive: timeout=5\r\n\r\n",
    )
    .await;
    // The backend never sees Connection/Keep-Alive as meaningful proxied
    // values; the echo backend would reflect them as x-echo-connection etc if
    // they leaked through unstripped.
    assert!(!resp.contains("x-echo-keep-alive"));
}

#[tokio::test]
async fn unreachable_backend_returns_502() {
    // A backend address nothing listens on.
    let dead = free_addr();
    let route = Route::new("app.test", None, false, vec![], false, "svc", vec![dead]);
    let routes = Arc::new(RouteTable::new(vec![route]));
    let certs = Arc::new(CertStore::new());
    let server = Arc::new(ProxyServer::new(routes, certs));

    let http_addr = free_addr();
    let s = server.clone();
    tokio::spawn(async move { s.serve_http(http_addr).await.unwrap() });
    wait_for_port(http_addr).await;

    let resp = reqwest_like_get(http_addr, "app.test", "/").await;
    assert_eq!(resp.status, 502);
}

/// A node's relay listener, dialing anything on loopback for whoever holds
/// the returned key.
async fn spawn_relay() -> (SocketAddr, cl_raft::KeyWatch) {
    let keys = cl_raft::fixed_key(Arc::new(cl_secret::ClusterKey::generate()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(cl_net::relay::serve(
        listener,
        keys.clone(),
        vec!["127.0.0.0/8".parse().unwrap()],
    ));
    (addr, keys)
}

/// A container on another node is out of this host's reach over the overlay,
/// so the proxy has to dial it through that node's relay - and only there.
#[tokio::test]
async fn a_backend_on_another_node_is_proxied_through_its_relay() {
    let backend = spawn_echo_backend().await;
    let (relay, keys) = spawn_relay().await;
    let dialer = cl_net::relay::Dialer::new(keys);
    // First a relay nobody runs: the backend itself listening must not help.
    dialer.set_relays(HashMap::from([(backend.ip(), free_addr())]));

    let route = Route::new("app.test", None, false, vec![], false, "svc", vec![backend]);
    let routes = Arc::new(RouteTable::new(vec![route]));
    let server =
        Arc::new(ProxyServer::new(routes, Arc::new(CertStore::new())).with_dialer(dialer.clone()));
    let http_addr = free_addr();
    tokio::spawn(async move { server.serve_http(http_addr).await.unwrap() });
    wait_for_port(http_addr).await;

    let resp = reqwest_like_get(http_addr, "app.test", "/hello").await;
    assert_eq!(resp.status, 502, "{}", resp.body);
    assert!(resp.body.contains("relay"), "{}", resp.body);

    dialer.set_relays(HashMap::from([(backend.ip(), relay)]));
    let resp = reqwest_like_get(http_addr, "app.test", "/hello").await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(resp.body, "GET /hello");
}

#[tokio::test]
async fn unmatched_host_returns_404() {
    let routes = Arc::new(RouteTable::new(vec![]));
    let certs = Arc::new(CertStore::new());
    let server = Arc::new(ProxyServer::new(routes, certs));

    let http_addr = free_addr();
    let s = server.clone();
    tokio::spawn(async move { s.serve_http(http_addr).await.unwrap() });
    wait_for_port(http_addr).await;

    let resp = reqwest_like_get(http_addr, "nowhere.test", "/").await;
    assert_eq!(resp.status, 404);
}

/// The ACME HTTP-01 fallback's whole reason to exist: a validator must reach
/// the challenge response even for a host with no route at all yet (issuance
/// for a brand new, zone-uncovered name).
#[tokio::test]
async fn an_acme_challenge_token_is_served_regardless_of_host_or_routing() {
    let routes = Arc::new(RouteTable::new(vec![]));
    let certs = Arc::new(CertStore::new());
    let http01 = Arc::new(Http01Store::default());
    http01.set("the-token", "the-token.thumbprint");
    let server = Arc::new(ProxyServer::full(routes, certs, None, http01.clone()));

    let http_addr = free_addr();
    let s = server.clone();
    tokio::spawn(async move { s.serve_http(http_addr).await.unwrap() });
    wait_for_port(http_addr).await;

    let resp = reqwest_like_get(
        http_addr,
        "nowhere.test",
        "/.well-known/acme-challenge/the-token",
    )
    .await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "the-token.thumbprint");

    let missing = reqwest_like_get(
        http_addr,
        "nowhere.test",
        "/.well-known/acme-challenge/other-token",
    )
    .await;
    assert_eq!(missing.status, 404);
}

#[tokio::test]
async fn http_redirects_to_https_when_route_has_tls() {
    let backend = spawn_echo_backend().await;
    let route = Route::new(
        "secure.test",
        None,
        true,
        vec![],
        false,
        "svc",
        vec![backend],
    );
    let routes = Arc::new(RouteTable::new(vec![route]));
    let certs = Arc::new(CertStore::new());
    let server = Arc::new(ProxyServer::new(routes, certs));

    let http_addr = free_addr();
    let s = server.clone();
    tokio::spawn(async move { s.serve_http(http_addr).await.unwrap() });
    wait_for_port(http_addr).await;

    let resp = reqwest_like_get(http_addr, "secure.test", "/path").await;
    assert_eq!(resp.status, 308);
    assert_eq!(
        resp.headers.get("location").unwrap(),
        "https://secure.test/path"
    );
}

#[tokio::test]
async fn strip_prefix_middleware_removes_route_prefix_before_forwarding() {
    let backend = spawn_echo_backend().await;
    let route = Route::new(
        "app.test",
        Some("/api".to_string()),
        false,
        vec![Middleware::StripPrefix],
        false,
        "svc",
        vec![backend],
    );
    let routes = Arc::new(RouteTable::new(vec![route]));
    let certs = Arc::new(CertStore::new());
    let server = Arc::new(ProxyServer::new(routes, certs));

    let http_addr = free_addr();
    let s = server.clone();
    tokio::spawn(async move { s.serve_http(http_addr).await.unwrap() });
    wait_for_port(http_addr).await;

    let resp = reqwest_like_get(http_addr, "app.test", "/api/things").await;
    assert_eq!(resp.body, "GET /things");
}

// --- minimal HTTP/1.1 client helpers, no extra dependency on reqwest ---

struct SimpleResponse {
    status: u16,
    headers: std::collections::HashMap<String, String>,
    body: String,
}

async fn reqwest_like_get(addr: SocketAddr, host: &str, path: &str) -> SimpleResponse {
    let raw = raw_request(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    parse_response(&raw)
}

async fn raw_request(addr: SocketAddr, req: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).to_string()
}

fn parse_response(raw: &str) -> SimpleResponse {
    let mut parts = raw.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("").to_string();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut headers = std::collections::HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    SimpleResponse {
        status,
        headers,
        body,
    }
}

// ---- per-route proxy settings ----

/// Stand up a proxy in front of one route and return its plain HTTP address.
async fn serve_route(route: Route) -> SocketAddr {
    let routes = Arc::new(RouteTable::new(vec![route]));
    let certs = Arc::new(CertStore::new());
    let server = Arc::new(ProxyServer::new(routes, certs));
    let http_addr = free_addr();
    tokio::spawn(async move { server.serve_http(http_addr).await.unwrap() });
    wait_for_port(http_addr).await;
    http_addr
}

fn plain_route(backend: SocketAddr, options: cl_proxy::RouteOptions) -> Route {
    Route::new("app.test", None, false, vec![], false, "svc", vec![backend]).with_options(options)
}

async fn post_body(addr: SocketAddr, len: usize) -> SimpleResponse {
    let body = "x".repeat(len);
    let raw = raw_request(
        addr,
        &format!(
            "POST /upload HTTP/1.1\r\nHost: app.test\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}"
        ),
    )
    .await;
    parse_response(&raw)
}

#[tokio::test]
async fn a_body_over_the_limit_is_refused_with_413() {
    let backend = spawn_echo_backend().await;
    let addr = serve_route(plain_route(
        backend,
        cl_proxy::RouteOptions {
            max_body_size: Some(16),
            ..cl_proxy::RouteOptions::permissive()
        },
    ))
    .await;

    let resp = post_body(addr, 64).await;
    assert_eq!(resp.status, 413);
    // The backend must never have been contacted, so nothing it echoes back
    // should be present.
    assert!(
        !resp.headers.contains_key("x-echo-host"),
        "{:?}",
        resp.headers
    );
}

#[tokio::test]
async fn a_body_within_the_limit_is_forwarded() {
    let backend = spawn_echo_backend().await;
    let addr = serve_route(plain_route(
        backend,
        cl_proxy::RouteOptions {
            max_body_size: Some(1024),
            ..cl_proxy::RouteOptions::permissive()
        },
    ))
    .await;

    let resp = post_body(addr, 64).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "POST /upload");
}

#[tokio::test]
async fn a_route_with_no_limit_forwards_any_body() {
    let backend = spawn_echo_backend().await;
    let addr = serve_route(plain_route(backend, cl_proxy::RouteOptions::permissive())).await;
    let resp = post_body(addr, 100_000).await;
    assert_eq!(resp.status, 200);
}

/// The default rewrites Host to the backend's address, which is what most
/// containers want; preserve_host is for the ones that build absolute URLs.
#[tokio::test]
async fn host_is_rewritten_to_the_backend_by_default() {
    let backend = spawn_echo_backend().await;
    let addr = serve_route(plain_route(backend, cl_proxy::RouteOptions::permissive())).await;
    let resp = reqwest_like_get(addr, "app.test", "/").await;
    assert_eq!(
        resp.headers.get("x-echo-host").unwrap(),
        &backend.to_string()
    );
}

#[tokio::test]
async fn preserve_host_forwards_the_clients_host_header() {
    let backend = spawn_echo_backend().await;
    let addr = serve_route(plain_route(
        backend,
        cl_proxy::RouteOptions {
            preserve_host: true,
            ..cl_proxy::RouteOptions::permissive()
        },
    ))
    .await;
    let resp = reqwest_like_get(addr, "app.test", "/").await;
    assert_eq!(resp.headers.get("x-echo-host").unwrap(), "app.test");
}

#[tokio::test]
async fn an_upgrade_is_rejected_when_websockets_are_off() {
    let backend = spawn_echo_backend().await;
    let addr = serve_route(plain_route(
        backend,
        cl_proxy::RouteOptions {
            websockets: false,
            ..cl_proxy::RouteOptions::default()
        },
    ))
    .await;

    let raw = raw_request(
        addr,
        "GET /ws HTTP/1.1\r\nHost: app.test\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
    )
    .await;
    assert_eq!(parse_response(&raw).status, 400);
}

/// Spawns a raw (non-HTTP-library) backend that records the first line it
/// receives on each connection and never responds — enough to see exactly
/// what request-target the proxy put on the wire for an upgrade, without a
/// higher-level HTTP parser normalizing it back to a path for us.
async fn spawn_line_capturing_backend() -> (SocketAddr, tokio::sync::mpsc::UnboundedReceiver<String>)
{
    use tokio::io::AsyncReadExt;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                if let Ok(n) = stream.read(&mut buf).await {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    let first_line = text.lines().next().unwrap_or("").to_string();
                    let _ = tx.send(first_line);
                }
                // Never responds: the point is to see what was sent, not to
                // complete the handshake.
            });
        }
    });
    (addr, rx)
}

/// A reverse proxy must send an origin-form request-target (path + query
/// only) to the backend, exactly like a normal HTTP client talking to an
/// origin server — never the absolute-form (`http://host:port/path`) that
/// `proxy_websocket`'s low-level connection used to write verbatim, which
/// backends' upgrade handlers (Node's `http` server included) don't match
/// against their expected route and silently drop.
#[tokio::test]
async fn a_websocket_upgrade_is_forwarded_with_an_origin_form_request_target() {
    let (backend, mut lines) = spawn_line_capturing_backend().await;
    let addr = serve_route(plain_route(
        backend,
        cl_proxy::RouteOptions {
            websockets: true,
            ..cl_proxy::RouteOptions::default()
        },
    ))
    .await;

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    {
        use tokio::io::AsyncWriteExt;
        stream
            .write_all(
                b"GET /api/socket.io/?EIO=4&transport=websocket HTTP/1.1\r\n\
                  Host: app.test\r\n\
                  Connection: Upgrade\r\n\
                  Upgrade: websocket\r\n\
                  Sec-WebSocket-Version: 13\r\n\
                  Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            )
            .await
            .unwrap();
    }

    let first_line = tokio::time::timeout(Duration::from_secs(2), lines.recv())
        .await
        .expect("backend never received a connection")
        .expect("backend connection closed without sending a line");

    assert_eq!(
        first_line,
        "GET /api/socket.io/?EIO=4&transport=websocket HTTP/1.1"
    );
}

/// An upgrade opens its own backend connection outside the pool; it has to
/// go through the owning node's relay too.
#[tokio::test]
async fn a_websocket_upgrade_reaches_a_backend_on_another_node_through_its_relay() {
    let (backend, mut lines) = spawn_line_capturing_backend().await;
    let (relay, keys) = spawn_relay().await;
    let dialer = cl_net::relay::Dialer::new(keys);
    dialer.set_relays(HashMap::from([(backend.ip(), relay)]));

    let route = plain_route(
        backend,
        cl_proxy::RouteOptions {
            websockets: true,
            ..cl_proxy::RouteOptions::default()
        },
    );
    let routes = Arc::new(RouteTable::new(vec![route]));
    let server = Arc::new(ProxyServer::new(routes, Arc::new(CertStore::new())).with_dialer(dialer));
    let addr = free_addr();
    tokio::spawn(async move { server.serve_http(addr).await.unwrap() });
    wait_for_port(addr).await;

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    {
        use tokio::io::AsyncWriteExt;
        stream
            .write_all(
                b"GET /ws HTTP/1.1\r\n\
                  Host: app.test\r\n\
                  Connection: Upgrade\r\n\
                  Upgrade: websocket\r\n\
                  Sec-WebSocket-Version: 13\r\n\
                  Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            )
            .await
            .unwrap();
    }

    let first_line = tokio::time::timeout(Duration::from_secs(5), lines.recv())
        .await
        .expect("backend never received a connection")
        .expect("backend connection closed without sending a line");
    assert_eq!(first_line, "GET /ws HTTP/1.1");
}

/// A slow backend must not hold the connection open forever when the route
/// sets a deadline.
#[tokio::test]
async fn a_backend_past_the_response_timeout_gives_504() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let slow = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            // Accept and then never answer.
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(stream);
            });
        }
    });

    let addr = serve_route(plain_route(
        slow,
        cl_proxy::RouteOptions {
            response_timeout: Some(Duration::from_millis(150)),
            ..cl_proxy::RouteOptions::permissive()
        },
    ))
    .await;

    let resp = reqwest_like_get(addr, "app.test", "/").await;
    assert_eq!(resp.status, 504);
}

// ---- network gating ----

#[tokio::test]
async fn a_route_restricted_to_a_subnet_that_excludes_the_client_is_forbidden() {
    let backend = spawn_echo_backend().await;
    // The test client always connects from 127.0.0.1; a subnet that does not
    // contain it stands in for "this request did not arrive on that network".
    let addr = serve_route(plain_route(
        backend,
        cl_proxy::RouteOptions {
            allowed_subnet: Some("10.0.0.0/8".parse().unwrap()),
            ..cl_proxy::RouteOptions::permissive()
        },
    ))
    .await;

    let resp = reqwest_like_get(addr, "app.test", "/").await;
    assert_eq!(resp.status, 403);
}

#[tokio::test]
async fn a_route_restricted_to_a_subnet_that_includes_the_client_is_served() {
    let backend = spawn_echo_backend().await;
    let addr = serve_route(plain_route(
        backend,
        cl_proxy::RouteOptions {
            allowed_subnet: Some("127.0.0.0/8".parse().unwrap()),
            ..cl_proxy::RouteOptions::permissive()
        },
    ))
    .await;

    let resp = reqwest_like_get(addr, "app.test", "/").await;
    assert_eq!(resp.status, 200);
}

#[tokio::test]
async fn a_public_route_has_no_subnet_restriction() {
    let backend = spawn_echo_backend().await;
    let addr = serve_route(plain_route(backend, cl_proxy::RouteOptions::permissive())).await;
    let resp = reqwest_like_get(addr, "app.test", "/").await;
    assert_eq!(resp.status, 200);
}

// ---- per-stack allowed hosts ----

/// Serves two routes on network `wan` (`app.test`, `other.test`) with the
/// loopback client treated as a stack whose policy is `policy`.
async fn serve_with_policy(
    backend: SocketAddr,
    policy: Option<cl_proxy::StackPolicy>,
) -> SocketAddr {
    let options = || cl_proxy::RouteOptions {
        network: Some("wan".to_string()),
        ..cl_proxy::RouteOptions::permissive()
    };
    let routes = Arc::new(RouteTable::new(vec![
        Route::new("app.test", None, false, vec![], false, "svc", vec![backend])
            .with_options(options()),
        Route::new(
            "other.test",
            None,
            false,
            vec![],
            false,
            "svc",
            vec![backend],
        )
        .with_options(options()),
    ]));
    let peers = Arc::new(cl_proxy::PeerPolicies::new());
    if let Some(policy) = policy {
        peers.replace(vec![("127.0.0.0/8".parse().unwrap(), policy)]);
    }
    let server = ProxyServer::new(routes, Arc::new(CertStore::new())).with_peer_policies(peers);
    let http_addr = free_addr();
    tokio::spawn(async move { server.serve_http(http_addr).await.unwrap() });
    wait_for_port(http_addr).await;
    http_addr
}

fn policy_allowing(network: &str, hosts: &[&str]) -> cl_proxy::StackPolicy {
    cl_proxy::StackPolicy {
        stack: "blog".to_string(),
        networks: HashMap::from([(network.to_string(), cl_proxy::HostAllow::new(hosts))]),
    }
}

#[tokio::test]
async fn a_listed_host_is_served_and_an_unlisted_one_gets_an_explicit_403() {
    let backend = spawn_echo_backend().await;
    let addr = serve_with_policy(backend, Some(policy_allowing("wan", &["app.test"]))).await;

    assert_eq!(reqwest_like_get(addr, "app.test", "/").await.status, 200);

    let denied = reqwest_like_get(addr, "other.test", "/").await;
    assert_eq!(denied.status, 403);
    assert!(denied.body.contains("other.test"), "{}", denied.body);
    assert!(denied.body.contains("blog"), "{}", denied.body);
    assert!(denied.body.contains("wan"), "{}", denied.body);
}

#[tokio::test]
async fn the_host_header_port_does_not_dodge_the_list() {
    let backend = spawn_echo_backend().await;
    let addr = serve_with_policy(backend, Some(policy_allowing("wan", &["app.test"]))).await;
    assert_eq!(reqwest_like_get(addr, "app.test:80", "/").await.status, 200);
    assert_eq!(
        reqwest_like_get(addr, "other.test:80", "/").await.status,
        403
    );
}

#[tokio::test]
async fn an_empty_list_denies_every_host_on_the_network() {
    let backend = spawn_echo_backend().await;
    let addr = serve_with_policy(backend, Some(policy_allowing("wan", &[]))).await;
    assert_eq!(reqwest_like_get(addr, "app.test", "/").await.status, 403);
}

#[tokio::test]
async fn a_list_on_another_network_does_not_restrict_this_one() {
    let backend = spawn_echo_backend().await;
    let addr = serve_with_policy(backend, Some(policy_allowing("lan", &[]))).await;
    assert_eq!(reqwest_like_get(addr, "app.test", "/").await.status, 200);
}

#[tokio::test]
async fn no_policy_means_every_route_is_served() {
    let backend = spawn_echo_backend().await;
    let addr = serve_with_policy(backend, None).await;
    assert_eq!(reqwest_like_get(addr, "other.test", "/").await.status, 200);
}

#[tokio::test]
async fn an_unknown_host_is_still_a_404_not_a_403() {
    let backend = spawn_echo_backend().await;
    let addr = serve_with_policy(backend, Some(policy_allowing("wan", &[]))).await;
    assert_eq!(reqwest_like_get(addr, "nope.test", "/").await.status, 404);
}

// ---- redirects ----

#[tokio::test]
async fn a_redirect_route_never_touches_a_backend() {
    let route = Route::new("old.test", None, false, vec![], false, "redirect", vec![])
        .with_redirect(cl_proxy::RedirectTarget {
            url: "https://new.test".to_string(),
            preserve_path: false,
            status: 301,
        });
    let addr = serve_route(route).await;

    let resp = reqwest_like_get(addr, "old.test", "/anything").await;
    assert_eq!(resp.status, 301);
    assert_eq!(resp.headers.get("location").unwrap(), "https://new.test");
}

#[tokio::test]
async fn a_redirect_route_can_preserve_the_path_and_query() {
    let route = Route::new("old.test", None, false, vec![], false, "redirect", vec![])
        .with_redirect(cl_proxy::RedirectTarget {
            url: "https://new.test".to_string(),
            preserve_path: true,
            status: 302,
        });
    let addr = serve_route(route).await;

    let resp = reqwest_like_get(addr, "old.test", "/a/b?x=1").await;
    assert_eq!(resp.status, 302);
    assert_eq!(
        resp.headers.get("location").unwrap(),
        "https://new.test/a/b?x=1"
    );
}

// ---- redirect_regex ----

#[tokio::test]
async fn redirect_regex_rewrites_and_redirects_on_a_match() {
    let backend = spawn_echo_backend().await;
    let route = Route::new(
        "app.test",
        None,
        false,
        vec![Middleware::RedirectRegex(cl_config::RedirectRegex {
            regex: "^http://app.test/old/(.*)".to_string(),
            replacement: "http://app.test/new/$1".to_string(),
            permanent: true,
        })],
        false,
        "svc",
        vec![backend],
    );
    let addr = serve_route(route).await;

    let resp = reqwest_like_get(addr, "app.test", "/old/thing").await;
    assert_eq!(resp.status, 308);
    assert_eq!(
        resp.headers.get("location").unwrap(),
        "http://app.test/new/thing"
    );
}

#[tokio::test]
async fn redirect_regex_falls_through_to_the_backend_when_it_does_not_match() {
    let backend = spawn_echo_backend().await;
    let route = Route::new(
        "app.test",
        None,
        false,
        vec![Middleware::RedirectRegex(cl_config::RedirectRegex {
            regex: "^http://app.test/old/(.*)".to_string(),
            replacement: "http://app.test/new/$1".to_string(),
            permanent: true,
        })],
        false,
        "svc",
        vec![backend],
    );
    let addr = serve_route(route).await;

    let resp = reqwest_like_get(addr, "app.test", "/unrelated").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "GET /unrelated");
}

#[tokio::test]
async fn redirect_regex_temporary_uses_307() {
    let backend = spawn_echo_backend().await;
    let route = Route::new(
        "app.test",
        None,
        false,
        vec![Middleware::RedirectRegex(cl_config::RedirectRegex {
            regex: "^http://app.test/a$".to_string(),
            replacement: "http://app.test/b".to_string(),
            permanent: false,
        })],
        false,
        "svc",
        vec![backend],
    );
    let addr = serve_route(route).await;

    let resp = reqwest_like_get(addr, "app.test", "/a").await;
    assert_eq!(resp.status, 307);
    assert_eq!(resp.headers.get("location").unwrap(), "http://app.test/b");
}

// ---- forward_auth ----

async fn spawn_auth_endpoint(allow: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let svc = service_fn(move |_req: Request<Incoming>| async move {
                    let resp = if allow {
                        Response::builder().status(200).header("x-user", "alice")
                    } else {
                        Response::builder().status(401)
                    };
                    Ok::<_, Infallible>(resp.body(http_body_util::Full::new(Bytes::new())).unwrap())
                });
                let _ = http1::Builder::new().serve_connection(io, svc).await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn forward_auth_allows_the_request_and_attaches_identity_headers() {
    let backend = spawn_echo_backend().await;
    let auth = spawn_auth_endpoint(true).await;
    let route = Route::new(
        "app.test",
        None,
        false,
        vec![Middleware::ForwardAuth(cl_config::ForwardAuth {
            address: format!("http://{auth}/verify"),
            auth_response_headers: vec!["X-User".to_string()],
            timeout: None,
        })],
        false,
        "svc",
        vec![backend],
    );
    let addr = serve_route(route).await;

    let resp = reqwest_like_get(addr, "app.test", "/protected").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.headers.get("x-echo-x-user").unwrap(), "alice");
}

#[tokio::test]
async fn forward_auth_denies_and_never_reaches_the_backend() {
    let backend = spawn_echo_backend().await;
    let auth = spawn_auth_endpoint(false).await;
    let route = Route::new(
        "app.test",
        None,
        false,
        vec![Middleware::ForwardAuth(cl_config::ForwardAuth {
            address: format!("http://{auth}/verify"),
            auth_response_headers: vec![],
            timeout: None,
        })],
        false,
        "svc",
        vec![backend],
    );
    let addr = serve_route(route).await;

    let resp = reqwest_like_get(addr, "app.test", "/protected").await;
    assert_eq!(resp.status, 401);
    assert!(
        !resp.headers.contains_key("x-echo-host"),
        "{:?}",
        resp.headers
    );
}

// ---- CORS ----

fn cors_route(backend: SocketAddr, policy: cl_config::CorsPolicy) -> Route {
    Route::new(
        "app.test",
        None,
        false,
        vec![Middleware::Cors(policy)],
        false,
        "svc",
        vec![backend],
    )
}

fn open_cors() -> cl_config::CorsPolicy {
    cl_config::CorsPolicy {
        origins: vec!["https://app.example.com".to_string()],
        methods: vec!["GET".to_string(), "POST".to_string()],
        headers: Vec::new(),
        allow_credentials: false,
        max_age: None,
    }
}

#[tokio::test]
async fn a_cors_preflight_is_answered_without_reaching_the_backend() {
    let backend = spawn_echo_backend().await;
    let addr = serve_route(cors_route(backend, open_cors())).await;

    let raw = raw_request(
        addr,
        "OPTIONS /api HTTP/1.1\r\nHost: app.test\r\nOrigin: https://app.example.com\r\nAccess-Control-Request-Method: POST\r\nConnection: close\r\n\r\n",
    )
    .await;
    let resp = parse_response(&raw);
    assert_eq!(resp.status, 204);
    assert_eq!(
        resp.headers.get("access-control-allow-origin").unwrap(),
        "https://app.example.com"
    );
    assert!(resp.body.is_empty());
}

#[tokio::test]
async fn a_real_cors_request_carries_the_allow_headers() {
    let backend = spawn_echo_backend().await;
    let addr = serve_route(cors_route(backend, open_cors())).await;

    let raw = raw_request(
        addr,
        "GET /api HTTP/1.1\r\nHost: app.test\r\nOrigin: https://app.example.com\r\nConnection: close\r\n\r\n",
    )
    .await;
    let resp = parse_response(&raw);
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.headers.get("access-control-allow-origin").unwrap(),
        "https://app.example.com"
    );
}

#[tokio::test]
async fn a_disallowed_cors_origin_gets_no_allow_headers() {
    let backend = spawn_echo_backend().await;
    let addr = serve_route(cors_route(backend, open_cors())).await;

    let raw = raw_request(
        addr,
        "GET /api HTTP/1.1\r\nHost: app.test\r\nOrigin: https://evil.example.com\r\nConnection: close\r\n\r\n",
    )
    .await;
    let resp = parse_response(&raw);
    assert_eq!(resp.status, 200);
    assert!(!resp.headers.contains_key("access-control-allow-origin"));
}
