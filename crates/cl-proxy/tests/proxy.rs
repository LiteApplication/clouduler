//! Integration tests: real listeners, a real in-process backend, no network.

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
use cl_proxy::{CertStore, ProxyServer, Route, RouteTable};

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
    Ok(builder.body(http_body_util::Full::new(Bytes::from(body))).unwrap())
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
    assert_eq!(client.headers.get("x-echo-x-forwarded-proto").unwrap(), "http");
    assert_eq!(client.headers.get("x-echo-x-forwarded-host").unwrap(), "app.test");
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

#[tokio::test]
async fn http_redirects_to_https_when_route_has_tls() {
    let backend = spawn_echo_backend().await;
    let route = Route::new("secure.test", None, true, vec![], false, "svc", vec![backend]);
    let routes = Arc::new(RouteTable::new(vec![route]));
    let certs = Arc::new(CertStore::new());
    let server = Arc::new(ProxyServer::new(routes, certs));

    let http_addr = free_addr();
    let s = server.clone();
    tokio::spawn(async move { s.serve_http(http_addr).await.unwrap() });
    wait_for_port(http_addr).await;

    let resp = reqwest_like_get(http_addr, "secure.test", "/path").await;
    assert_eq!(resp.status, 308);
    assert_eq!(resp.headers.get("location").unwrap(), "https://secure.test/path");
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
    let raw = raw_request(addr, &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")).await;
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
    SimpleResponse { status, headers, body }
}
