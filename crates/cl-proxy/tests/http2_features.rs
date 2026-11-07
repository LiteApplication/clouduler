//! HTTP/2 feature coverage for the proxy, following RFC 9113 and the gRPC
//! over HTTP/2 spec: multiplexing, flow control with large bodies, pseudo
//! headers, connection-specific headers, HPACK, cancellation (RST_STREAM),
//! stream errors, graceful shutdown (GOAWAY) of an upstream connection,
//! header and trailer handling, gating, ALPN edge cases and gRPC framing.
//!
//! Not covered because the stack has no such feature: server push (hyper does
//! not implement it), stream priorities (deprecated by RFC 9113) and
//! extended CONNECT / websockets over h2 (RFC 8441 is not enabled; websockets
//! use HTTP/1.1, see `http2.rs`). Ping keep-alive runs on 30s timers and is
//! not exercised here.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use cl_proxy::{
    ensure_crypto_provider, CertStore, ProxyServer, RedirectTarget, Route, RouteOptions, RouteTable,
};
use cl_raft::CertificateState;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{HeaderMap, Request, Response, StatusCode, Version};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify};
use tokio_rustls::client::TlsStream;

const HOST: &str = "app.test";

type BoxErr = Box<dyn std::error::Error + Send + Sync>;
type TBody = BoxBody<Bytes, BoxErr>;

// ---- plumbing ------------------------------------------------------------

fn free_addr() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

async fn wait_for_port(addr: SocketAddr) {
    for _ in 0..100 {
        if TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("nothing listening on {addr}");
}

fn make_cert() -> CertificateState {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec![HOST.to_string()]).unwrap();
    CertificateState {
        fqdn: HOST.to_string(),
        chain_pem: cert.pem(),
        key_pem: signing_key.serialize_pem(),
        not_after: 0,
        issued_at: 0,
    }
}

struct Proxy {
    https: SocketAddr,
    cert: CertificateState,
}

async fn serve(routes: Vec<Route>) -> Proxy {
    ensure_crypto_provider();
    let cert = make_cert();
    let certs = Arc::new(CertStore::new());
    certs.load(std::slice::from_ref(&cert));
    let server = Arc::new(ProxyServer::new(Arc::new(RouteTable::new(routes)), certs));
    let https = free_addr();
    tokio::spawn(async move { server.serve_https(https).await.unwrap() });
    wait_for_port(https).await;
    Proxy { https, cert }
}

fn route(path: Option<&str>, backend: SocketAddr, options: RouteOptions) -> Route {
    Route::new(
        HOST,
        path.map(str::to_string),
        true,
        vec![],
        false,
        "svc",
        vec![backend],
    )
    .with_options(options)
}

fn h1_route(backend: SocketAddr) -> Route {
    route(None, backend, RouteOptions::permissive())
}

fn h2c_route(backend: SocketAddr) -> Route {
    route(
        None,
        backend,
        RouteOptions {
            http2: true,
            ..RouteOptions::permissive()
        },
    )
}

async fn try_tls_connect(proxy: &Proxy, alpn: &[&[u8]]) -> std::io::Result<TlsStream<TcpStream>> {
    let mut roots = rustls::RootCertStore::empty();
    let (_, pem) = x509_parser::pem::parse_x509_pem(proxy.cert.chain_pem.as_bytes()).unwrap();
    roots.add(CertificateDer::from(pem.contents)).unwrap();
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let stream = TcpStream::connect(proxy.https).await.unwrap();
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from(HOST).unwrap(), stream)
        .await
}

async fn tls_connect(proxy: &Proxy, alpn: &[&[u8]]) -> TlsStream<TcpStream> {
    try_tls_connect(proxy, alpn).await.unwrap()
}

type H2 = hyper::client::conn::http2::SendRequest<TBody>;
type H1 = hyper::client::conn::http1::SendRequest<TBody>;

async fn h2_client(proxy: &Proxy) -> H2 {
    let tls = tls_connect(proxy, &[b"h2", b"http/1.1"]).await;
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    let (sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
            .await
            .unwrap();
    tokio::spawn(conn);
    sender
}

async fn h1_client(proxy: &Proxy, alpn: &[&[u8]]) -> H1 {
    let tls = tls_connect(proxy, alpn).await;
    let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    tokio::spawn(conn);
    sender
}

fn full(b: impl Into<Bytes>) -> TBody {
    Full::new(b.into()).map_err(|n| match n {}).boxed()
}

fn empty() -> TBody {
    Empty::<Bytes>::new().map_err(|n| match n {}).boxed()
}

/// A body fed by a channel; items may be errors, to abort a stream.
struct Chan(mpsc::Receiver<Result<Frame<Bytes>, BoxErr>>);

impl futures::Stream for Chan {
    type Item = Result<Frame<Bytes>, BoxErr>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

fn channel_body() -> (mpsc::Sender<Result<Frame<Bytes>, BoxErr>>, TBody) {
    let (tx, rx) = mpsc::channel(8);
    (tx, StreamBody::new(Chan(rx)).boxed())
}

fn data(b: &'static [u8]) -> Result<Frame<Bytes>, BoxErr> {
    Ok(Frame::data(Bytes::from_static(b)))
}

/// An h2 request: absolute URI so the client sends `:authority`.
fn h2_req(method: &str, path: &str, headers: &[(&str, &str)], body: TBody) -> Request<TBody> {
    let mut b = Request::builder()
        .method(method)
        .uri(format!("https://{HOST}{path}"));
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(body).unwrap()
}

/// An HTTP/1.1 request: origin-form target and a Host header.
fn h1_req(method: &str, path: &str, headers: &[(&str, &str)], body: TBody) -> Request<TBody> {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("host", HOST);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(body).unwrap()
}

async fn text(resp: Response<Incoming>) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A backend taking HTTP/1.1 and h2c on one port (like NetBird's). Returns
/// its address and how many TCP connections it has accepted.
async fn spawn_backend<H, Fut>(handler: H) -> (SocketAddr, Arc<AtomicUsize>)
where
    H: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<TBody>> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let conns = Arc::new(AtomicUsize::new(0));
    let counter = conns.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let handler = handler.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req| {
                    let handler = handler.clone();
                    async move { Ok::<_, Infallible>(handler(req).await) }
                });
                let _ = auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    (addr, conns)
}

fn ok(body: TBody) -> Response<TBody> {
    Response::new(body)
}

/// Every request header, one `name: value` line each, plus the protocol and
/// target the backend saw.
async fn describe(req: Request<Incoming>) -> Response<TBody> {
    let mut out = format!("{:?} {} {}\n", req.version(), req.method(), req.uri());
    for (k, v) in req.headers() {
        out.push_str(&format!("{k}: {}\n", v.to_str().unwrap_or("?")));
    }
    ok(full(out))
}

fn header_line<'a>(described: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}: ");
    described.lines().find_map(|l| l.strip_prefix(&prefix))
}

/// Order-independent running hash, so a body can be verified however it was
/// chunked in flight.
fn roll(mut h: u64, data: &[u8]) -> u64 {
    for b in data {
        h = h.wrapping_mul(31).wrapping_add(u64::from(*b));
    }
    h
}

fn pattern(offset: usize, len: usize) -> Vec<u8> {
    (offset..offset + len).map(|i| (i % 251) as u8).collect()
}

// ---- multiplexing and connection reuse ----------------------------------

#[tokio::test]
async fn many_streams_share_one_client_connection_and_run_concurrently() {
    let (backend, _) = spawn_backend(|_| async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        ok(full("done"))
    })
    .await;
    let proxy = serve(vec![h1_route(backend)]).await;
    let client = h2_client(&proxy).await;

    let started = Instant::now();
    let tasks: Vec<_> = (0..10)
        .map(|i| {
            let mut client = client.clone();
            tokio::spawn(async move {
                let resp = client
                    .send_request(h2_req("GET", &format!("/slow/{i}"), &[], empty()))
                    .await
                    .unwrap();
                assert_eq!(resp.status(), 200);
                text(resp).await
            })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap(), "done");
    }
    // Ten 300ms requests one after the other would take 3s.
    assert!(
        started.elapsed() < Duration::from_millis(2000),
        "streams were serialised: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn concurrent_requests_to_an_h2c_route_share_one_upstream_connection() {
    let (backend, conns) = spawn_backend(|_| async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        ok(full("done"))
    })
    .await;
    let proxy = serve(vec![h2c_route(backend)]).await;
    let client = h2_client(&proxy).await;

    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let mut client = client.clone();
            tokio::spawn(async move {
                client
                    .send_request(h2_req("GET", "/x", &[], empty()))
                    .await
                    .unwrap()
                    .status()
            })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    assert_eq!(
        conns.load(Ordering::SeqCst),
        1,
        "h2c upstream must multiplex over one connection"
    );
}

// ---- flow control and large bodies --------------------------------------

/// A backend with `POST /up` (reports length and hash of what it received)
/// and `GET /down?n=N` (streams N patterned bytes).
async fn spawn_bulk_backend() -> SocketAddr {
    spawn_backend(|req: Request<Incoming>| async move {
        if req.uri().path() == "/up" {
            let mut body = req.into_body();
            let (mut len, mut hash) = (0usize, 0u64);
            while let Some(frame) = body.frame().await {
                if let Some(d) = frame.unwrap().data_ref() {
                    len += d.len();
                    hash = roll(hash, d);
                }
            }
            return ok(full(format!("len={len} hash={hash}")));
        }
        let n: usize = req
            .uri()
            .query()
            .and_then(|q| q.strip_prefix("n="))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let (tx, body) = channel_body();
        tokio::spawn(async move {
            let mut sent = 0;
            while sent < n {
                let take = (n - sent).min(65_536);
                let chunk = Bytes::from(pattern(sent, take));
                if tx.send(Ok(Frame::data(chunk))).await.is_err() {
                    return;
                }
                sent += take;
            }
        });
        ok(body)
    })
    .await
    .0
}

async fn assert_bulk_round_trip(proxy: &Proxy, total: usize) {
    let mut client = h2_client(proxy).await;

    // Upload: far beyond the 64KiB initial window, so it only completes if
    // WINDOW_UPDATEs flow through the proxy in both hops.
    let (tx, body) = channel_body();
    tokio::spawn(async move {
        let mut sent = 0;
        while sent < total {
            let take = (total - sent).min(65_536);
            let _ = tx
                .send(Ok(Frame::data(Bytes::from(pattern(sent, take)))))
                .await;
            sent += take;
        }
    });
    let resp = client
        .send_request(h2_req("POST", "/up", &[], body))
        .await
        .unwrap();
    let expected = roll(0, &pattern(0, total));
    assert_eq!(text(resp).await, format!("len={total} hash={expected}"));

    // Download of the same size, hashed as it arrives.
    let resp = client
        .send_request(h2_req("GET", &format!("/down?n={total}"), &[], empty()))
        .await
        .unwrap();
    let mut body = resp.into_body();
    let (mut len, mut hash) = (0usize, 0u64);
    while let Some(frame) = body.frame().await {
        if let Some(d) = frame.unwrap().data_ref() {
            len += d.len();
            hash = roll(hash, d);
        }
    }
    assert_eq!((len, hash), (total, expected));
}

#[tokio::test]
async fn multi_megabyte_bodies_cross_flow_control_over_http11_upstream() {
    let backend = spawn_bulk_backend().await;
    let proxy = serve(vec![h1_route(backend)]).await;
    assert_bulk_round_trip(&proxy, 6 * 1024 * 1024).await;
}

#[tokio::test]
async fn multi_megabyte_bodies_cross_flow_control_over_h2c_upstream() {
    let backend = spawn_bulk_backend().await;
    let proxy = serve(vec![h2c_route(backend)]).await;
    assert_bulk_round_trip(&proxy, 6 * 1024 * 1024).await;
}

// ---- messages without a body, status codes ------------------------------

#[tokio::test]
async fn bodiless_statuses_and_head_requests_pass_through_both_upstream_protocols() {
    let (backend, _) = spawn_backend(|req: Request<Incoming>| async move {
        match req.uri().path() {
            "/204" => Response::builder().status(204).body(empty()).unwrap(),
            "/304" => Response::builder()
                .status(304)
                .header("etag", "\"abc\"")
                .body(empty())
                .unwrap(),
            "/404" => Response::builder().status(404).body(full("nope")).unwrap(),
            _ => Response::builder()
                .header("content-length", "10")
                .body(empty())
                .unwrap(),
        }
    })
    .await;
    for r in [h1_route(backend), h2c_route(backend)] {
        let http2 = r.options.http2;
        let proxy = serve(vec![r]).await;
        let mut client = h2_client(&proxy).await;

        let resp = client
            .send_request(h2_req("GET", "/204", &[], empty()))
            .await
            .unwrap();
        assert_eq!(resp.status(), 204, "http2={http2}");
        assert!(text(resp).await.is_empty());

        let resp = client
            .send_request(h2_req("GET", "/304", &[], empty()))
            .await
            .unwrap();
        assert_eq!(resp.status(), 304);
        assert_eq!(resp.headers()["etag"], "\"abc\"");

        let resp = client
            .send_request(h2_req("GET", "/404", &[], empty()))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        assert_eq!(text(resp).await, "nope");

        let resp = client
            .send_request(h2_req("HEAD", "/head", &[], empty()))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-length"], "10");
        assert!(text(resp).await.is_empty());
    }
}

// ---- pseudo headers and routing -----------------------------------------

#[tokio::test]
async fn authority_becomes_the_host_the_route_and_backend_see() {
    let (backend, _) = spawn_backend(describe).await;
    // preserve_host so the backend reports what the proxy resolved.
    let proxy = serve(vec![route(
        None,
        backend,
        RouteOptions {
            preserve_host: true,
            ..RouteOptions::permissive()
        },
    )])
    .await;
    let mut client = h2_client(&proxy).await;

    let resp = client
        .send_request(h2_req("GET", "/p?q=1", &[], empty()))
        .await
        .unwrap();
    let seen = text(resp).await;
    assert_eq!(header_line(&seen, "host"), Some(HOST), "{seen}");
    assert!(seen.starts_with("HTTP/1.1 GET /p?q=1"), "{seen}");
    assert_eq!(header_line(&seen, "x-forwarded-host"), Some(HOST));
    assert_eq!(header_line(&seen, "x-forwarded-proto"), Some("https"));
    assert_eq!(header_line(&seen, "x-forwarded-for"), Some("127.0.0.1"));
}

#[tokio::test]
async fn an_authority_with_a_port_still_matches_its_route() {
    let (backend, _) = spawn_backend(describe).await;
    let proxy = serve(vec![h1_route(backend)]).await;
    let mut client = h2_client(&proxy).await;
    let req = Request::builder()
        .uri(format!("https://{HOST}:8443/x"))
        .body(empty())
        .unwrap();
    assert_eq!(client.send_request(req).await.unwrap().status(), 200);
}

#[tokio::test]
async fn an_unknown_authority_is_a_404_and_authority_beats_a_conflicting_host_header() {
    let (backend, _) = spawn_backend(describe).await;
    let proxy = serve(vec![h1_route(backend)]).await;
    let mut client = h2_client(&proxy).await;

    let req = Request::builder()
        .uri("https://nowhere.test/x")
        .body(empty())
        .unwrap();
    assert_eq!(client.send_request(req).await.unwrap().status(), 404);

    // `:authority` names an unrouted host while `Host` names a routed one.
    // RFC 9113 8.3.1: the proxy must act on `:authority`.
    let req = Request::builder()
        .uri("https://nowhere.test/x")
        .header("host", HOST)
        .body(empty())
        .unwrap();
    assert_eq!(
        client.send_request(req).await.unwrap().status(),
        404,
        "a Host header must not override :authority"
    );
}

// ---- connection-specific headers (RFC 9113 8.2.2) -----------------------

#[tokio::test]
async fn connection_specific_request_headers_do_not_reach_either_upstream() {
    let (backend, _) = spawn_backend(describe).await;
    for r in [h1_route(backend), h2c_route(backend)] {
        let proxy = serve(vec![r]).await;
        // An HTTP/1.1 client may send these; they are hop-by-hop.
        let mut client = h1_client(&proxy, &[b"http/1.1"]).await;
        let resp = client
            .send_request(h1_req(
                "GET",
                "/x",
                &[
                    ("proxy-connection", "keep-alive"),
                    ("keep-alive", "timeout=5"),
                    ("connection", "keep-alive, x-private"),
                    ("x-private", "secret"),
                    ("te", "gzip"),
                    ("x-end-to-end", "kept"),
                ],
                empty(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let seen = text(resp).await;
        for gone in [
            "proxy-connection",
            "keep-alive",
            "connection",
            "x-private",
            "te",
        ] {
            assert_eq!(header_line(&seen, gone), None, "{gone} leaked: {seen}");
        }
        assert_eq!(header_line(&seen, "x-end-to-end"), Some("kept"), "{seen}");
    }
}

/// An HTTP/1.1 backend answering with headers that are illegal on h2. If the
/// proxy forwarded them the h2 client would reset the stream.
#[tokio::test]
async fn connection_specific_response_headers_are_removed_for_an_h2_client() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                s.write_all(
                    b"HTTP/1.1 200 OK\r\nConnection: keep-alive\r\nKeep-Alive: timeout=5\r\n\
                      Proxy-Connection: keep-alive\r\nTransfer-Encoding: chunked\r\n\
                      X-Kept: yes\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
                )
                .await
                .unwrap();
            });
        }
    });
    let proxy = serve(vec![h1_route(backend)]).await;
    let mut client = h2_client(&proxy).await;
    let resp = client
        .send_request(h2_req("GET", "/x", &[], empty()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    for gone in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "transfer-encoding",
    ] {
        assert!(!resp.headers().contains_key(gone), "{gone} leaked");
    }
    assert_eq!(resp.headers()["x-kept"], "yes");
    assert_eq!(text(resp).await, "hello");
}

#[tokio::test]
async fn repeated_response_headers_survive_the_translation_to_h2() {
    let (backend, _) = spawn_backend(|_| async {
        Response::builder()
            .header("set-cookie", "a=1; Path=/")
            .header("set-cookie", "b=2; Path=/")
            .body(full("x"))
            .unwrap()
    })
    .await;
    for r in [h1_route(backend), h2c_route(backend)] {
        let proxy = serve(vec![r]).await;
        let mut client = h2_client(&proxy).await;
        let resp = client
            .send_request(h2_req("GET", "/x", &[], empty()))
            .await
            .unwrap();
        let cookies: Vec<_> = resp.headers().get_all("set-cookie").iter().collect();
        assert_eq!(cookies.len(), 2, "{cookies:?}");
    }
}

// ---- HPACK, header sizes, end-to-end headers ----------------------------

#[tokio::test]
async fn large_and_repeated_headers_survive_hpack_on_a_reused_connection() {
    let (backend, _) = spawn_backend(describe).await;
    let proxy = serve(vec![h1_route(backend)]).await;
    let mut client = h2_client(&proxy).await;

    let big = "v".repeat(8 * 1024);
    // The same large header twice exercises the dynamic table (the second
    // request is sent as a table reference); then many distinct headers.
    for _ in 0..2 {
        let resp = client
            .send_request(h2_req("GET", "/x", &[("x-big", big.as_str())], empty()))
            .await
            .unwrap();
        let seen = text(resp).await;
        assert_eq!(header_line(&seen, "x-big"), Some(big.as_str()));
    }
    let names: Vec<String> = (0..60).map(|i| format!("x-h{i}")).collect();
    let pairs: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "val")).collect();
    let resp = client
        .send_request(h2_req("GET", "/x", &pairs, empty()))
        .await
        .unwrap();
    let seen = text(resp).await;
    for n in &names {
        assert_eq!(header_line(&seen, n), Some("val"), "{n}");
    }
}

#[tokio::test]
async fn grpc_request_metadata_is_forwarded_untouched_to_an_h2c_upstream() {
    let (backend, _) = spawn_backend(describe).await;
    let proxy = serve(vec![h2c_route(backend)]).await;
    let mut client = h2_client(&proxy).await;
    let resp = client
        .send_request(h2_req(
            "POST",
            "/management.ManagementService/Sync",
            &[
                ("content-type", "application/grpc"),
                ("te", "trailers"),
                ("grpc-timeout", "30S"),
                ("grpc-encoding", "gzip"),
                ("grpc-accept-encoding", "gzip,identity"),
                ("user-agent", "grpc-go/1.60"),
                ("authorization", "Bearer abc"),
                ("x-bin-meta-bin", "AAEC"),
            ],
            empty(),
        ))
        .await
        .unwrap();
    let seen = text(resp).await;
    assert!(seen.starts_with("HTTP/2.0 POST"), "{seen}");
    for (k, v) in [
        ("content-type", "application/grpc"),
        ("te", "trailers"),
        ("grpc-timeout", "30S"),
        ("grpc-encoding", "gzip"),
        ("grpc-accept-encoding", "gzip,identity"),
        ("user-agent", "grpc-go/1.60"),
        ("authorization", "Bearer abc"),
        ("x-bin-meta-bin", "AAEC"),
    ] {
        assert_eq!(header_line(&seen, k), Some(v), "{k}: {seen}");
    }
}

#[tokio::test]
async fn te_is_not_invented_for_a_client_that_did_not_send_it() {
    let (backend, _) = spawn_backend(describe).await;
    let proxy = serve(vec![h2c_route(backend)]).await;
    let mut client = h2_client(&proxy).await;
    let resp = client
        .send_request(h2_req("GET", "/x", &[], empty()))
        .await
        .unwrap();
    assert_eq!(header_line(&text(resp).await, "te"), None);
}

// ---- gRPC framing, trailers-only, binary safety -------------------------

#[tokio::test]
async fn length_prefixed_grpc_messages_cross_the_proxy_byte_for_byte() {
    // Echo every frame as it arrives.
    let (backend, _) = spawn_backend(|req: Request<Incoming>| async move {
        let (tx, body) = channel_body();
        let mut input = req.into_body();
        tokio::spawn(async move {
            while let Some(Ok(frame)) = input.frame().await {
                if tx.send(Ok(frame)).await.is_err() {
                    return;
                }
            }
        });
        Response::builder()
            .header("content-type", "application/grpc")
            .body(body)
            .unwrap()
    })
    .await;
    let proxy = serve(vec![h2c_route(backend)]).await;
    let mut client = h2_client(&proxy).await;

    // Two messages: flag 0 + big-endian length + payload, with NUL bytes and
    // split across data frames at awkward places.
    let msg_a = [&[0u8, 0, 0, 0, 5][..], b"he\0lo"].concat();
    let msg_b = [&[0u8, 0, 0, 0, 3][..], &[0xff, 0x00, 0x7f]].concat();
    let wire = [msg_a.clone(), msg_b.clone()].concat();
    let (tx, body) = channel_body();
    let resp = client
        .send_request(h2_req(
            "POST",
            "/signalexchange.SignalExchange/Send",
            &[("content-type", "application/grpc"), ("te", "trailers")],
            body,
        ))
        .await
        .unwrap();
    for piece in [&wire[..1], &wire[1..7], &wire[7..]] {
        tx.send(Ok(Frame::data(Bytes::copy_from_slice(piece))))
            .await
            .unwrap();
    }
    drop(tx);
    assert_eq!(resp.headers()["content-type"], "application/grpc");
    let got = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(got.as_ref(), wire.as_slice());
}

#[tokio::test]
async fn a_trailers_only_grpc_error_response_reaches_the_client_intact() {
    let (backend, _) = spawn_backend(|_| async {
        // Status in the header block itself, no body, as grpc-go does for a
        // call that fails before producing a message.
        Response::builder()
            .header("content-type", "application/grpc")
            .header("grpc-status", "12")
            .header("grpc-message", "unknown%20service")
            .body(empty())
            .unwrap()
    })
    .await;
    let proxy = serve(vec![h2c_route(backend)]).await;
    let mut client = h2_client(&proxy).await;
    let resp = client
        .send_request(h2_req(
            "POST",
            "/nope.Service/Method",
            &[("content-type", "application/grpc"), ("te", "trailers")],
            empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["grpc-status"], "12");
    assert_eq!(resp.headers()["grpc-message"], "unknown%20service");
}

#[tokio::test]
async fn response_trailers_follow_streamed_messages_in_order() {
    let (backend, _) = spawn_backend(|_| async {
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", "0".parse().unwrap());
        trailers.insert("grpc-message", "ok".parse().unwrap());
        let body = StreamBody::new(futures::stream::iter(vec![
            data(b"one"),
            data(b"two"),
            Ok(Frame::trailers(trailers)),
        ]))
        .boxed();
        Response::builder()
            .header("content-type", "application/grpc")
            .body(body)
            .unwrap()
    })
    .await;
    for r in [h1_route(backend), h2c_route(backend)] {
        let http2 = r.options.http2;
        if !http2 {
            continue; // trailers over an HTTP/1.1 upstream are chunked-only
        }
        let proxy = serve(vec![r]).await;
        let mut client = h2_client(&proxy).await;
        let resp = client
            .send_request(h2_req("POST", "/x", &[("te", "trailers")], empty()))
            .await
            .unwrap();
        let collected = resp.into_body().collect().await.unwrap();
        assert_eq!(collected.trailers().unwrap()["grpc-message"], "ok");
        assert_eq!(collected.to_bytes().as_ref(), b"onetwo");
    }
}

// ---- cancellation (RST_STREAM) and stream errors ------------------------

/// `/stream` sends a tick every 50ms and signals `closed` once nobody is
/// reading it any more.
async fn spawn_ticker_backend() -> (SocketAddr, Arc<Notify>) {
    let closed = Arc::new(Notify::new());
    let signal = closed.clone();
    let (addr, _) = spawn_backend(move |_| {
        let signal = signal.clone();
        async move {
            let (tx, body) = channel_body();
            tokio::spawn(async move {
                loop {
                    if tx.send(data(b"tick")).await.is_err() {
                        signal.notify_one();
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            });
            ok(body)
        }
    })
    .await;
    (addr, closed)
}

#[tokio::test]
async fn a_client_stream_reset_cancels_the_upstream_stream() {
    let (backend, closed) = spawn_ticker_backend().await;
    for r in [h2c_route(backend), h1_route(backend)] {
        let http2 = r.options.http2;
        let proxy = serve(vec![r]).await;
        let mut client = h2_client(&proxy).await;
        let resp = client
            .send_request(h2_req("GET", "/stream", &[], empty()))
            .await
            .unwrap();
        let mut body = resp.into_body();
        let first = body.frame().await.unwrap().unwrap();
        assert_eq!(first.into_data().unwrap(), Bytes::from_static(b"tick"));
        // Dropping the body resets the stream (RST_STREAM CANCEL).
        drop(body);
        tokio::time::timeout(Duration::from_secs(5), closed.notified())
            .await
            .unwrap_or_else(|_| {
                panic!("upstream kept producing after the client cancelled (http2={http2})")
            });
    }
}

#[tokio::test]
async fn a_client_disconnect_cancels_the_upstream_stream() {
    let (backend, closed) = spawn_ticker_backend().await;
    let proxy = serve(vec![h2c_route(backend)]).await;
    let mut client = h2_client(&proxy).await;
    let resp = client
        .send_request(h2_req("GET", "/stream", &[], empty()))
        .await
        .unwrap();
    let mut body = resp.into_body();
    body.frame().await.unwrap().unwrap();
    drop(body);
    drop(client); // closes the whole connection
    tokio::time::timeout(Duration::from_secs(5), closed.notified())
        .await
        .expect("upstream kept producing after the client went away");
}

#[tokio::test]
async fn an_upstream_failure_mid_body_is_an_error_not_a_clean_end() {
    // The error must come after the headers went out, or the proxy would
    // see a failed request rather than a failed body.
    let (backend, _) = spawn_backend(|_| async {
        let (tx, body) = channel_body();
        tokio::spawn(async move {
            let _ = tx.send(data(b"partial")).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = tx
                .send(Err::<Frame<Bytes>, BoxErr>(Box::new(
                    std::io::Error::other("backend died"),
                )))
                .await;
        });
        Response::builder()
            .header("content-length", "1000")
            .body(body)
            .unwrap()
    })
    .await;
    for r in [h2c_route(backend), h1_route(backend)] {
        let http2 = r.options.http2;
        let proxy = serve(vec![r]).await;
        let mut client = h2_client(&proxy).await;
        let resp = client
            .send_request(h2_req("GET", "/boom", &[], empty()))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "http2={http2}");
        let result = resp.into_body().collect().await;
        assert!(
            result.is_err(),
            "a truncated response must not look complete (http2={http2})"
        );
        // The connection and other streams are unaffected.
        let ok_resp = client
            .send_request(h2_req("GET", "/boom", &[], empty()))
            .await;
        assert!(ok_resp.is_ok(), "connection died with one stream");
    }
}

#[tokio::test]
async fn an_h2c_route_to_a_dead_or_http11_only_backend_is_a_502() {
    // Nothing listening.
    let dead = free_addr();
    let proxy = serve(vec![h2c_route(dead)]).await;
    let mut client = h2_client(&proxy).await;
    let resp = client
        .send_request(h2_req("GET", "/x", &[], empty()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);

    // A server that only speaks HTTP/1.1 cannot answer prior-knowledge h2.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let h1_only = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let svc = service_fn(|_| async {
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"h1"))))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    let proxy = serve(vec![h2c_route(h1_only)]).await;
    let mut client = h2_client(&proxy).await;
    let resp = tokio::time::timeout(
        Duration::from_secs(10),
        client.send_request(h2_req("GET", "/x", &[], empty())),
    )
    .await
    .expect("request hung")
    .unwrap();
    assert_eq!(resp.status(), 502);
}

// ---- upstream GOAWAY / connection churn ---------------------------------

/// Closes each upstream h2 connection gracefully (GOAWAY) after answering one
/// request. The pooled connection in the proxy goes stale between requests.
#[tokio::test]
async fn an_upstream_that_sends_goaway_after_each_request_is_redialled() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend = listener.local_addr().unwrap();
    let conns = Arc::new(AtomicUsize::new(0));
    let counter = conns.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let answered = Arc::new(Notify::new());
                let signal = answered.clone();
                let svc = service_fn(move |_| {
                    let signal = signal.clone();
                    async move {
                        signal.notify_one();
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
                    }
                });
                let builder = auto::Builder::new(TokioExecutor::new());
                let conn = builder.serve_connection(TokioIo::new(stream), svc);
                tokio::pin!(conn);
                tokio::select! {
                    _ = conn.as_mut() => {}
                    _ = answered.notified() => {
                        conn.as_mut().graceful_shutdown();
                        let _ = conn.await;
                    }
                }
            });
        }
    });
    let proxy = serve(vec![h2c_route(backend)]).await;
    let mut client = h2_client(&proxy).await;
    for i in 0..4 {
        let resp = client
            .send_request(h2_req("GET", "/x", &[], empty()))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "request {i} failed after upstream GOAWAY"
        );
        assert_eq!(text(resp).await, "ok");
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert!(conns.load(Ordering::SeqCst) >= 2, "expected redials");
}

// ---- request body limits over h2 ----------------------------------------

#[tokio::test]
async fn max_body_size_applies_to_h2_requests_declared_and_streamed() {
    let (backend, _) = spawn_bulk_backend_counting().await;
    let proxy = serve(vec![route(
        None,
        backend,
        RouteOptions {
            max_body_size: Some(1000),
            ..RouteOptions::permissive()
        },
    )])
    .await;
    let mut client = h2_client(&proxy).await;

    let resp = client
        .send_request(h2_req(
            "POST",
            "/up",
            &[("content-length", "5000")],
            full(vec![b'x'; 5000]),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);

    // No content-length: caught while streaming. It must not succeed with the
    // whole body delivered.
    let (tx, body) = channel_body();
    let sent = tokio::spawn(async move {
        for _ in 0..5 {
            if tx
                .send(Ok(Frame::data(Bytes::from(vec![b'x'; 1000]))))
                .await
                .is_err()
            {
                return;
            }
        }
    });
    let outcome = client.send_request(h2_req("POST", "/up", &[], body)).await;
    let _ = sent.await;
    match outcome {
        Err(_) => {}
        Ok(resp) => {
            let status = resp.status();
            let body = resp.into_body().collect().await;
            let delivered_all = status == 200
                && body.is_ok_and(|b| String::from_utf8_lossy(&b.to_bytes()) == "len=5000");
            assert!(!delivered_all, "an over-limit stream got through");
        }
    }

    let resp = client
        .send_request(h2_req("POST", "/up", &[], full(vec![b'x'; 900])))
        .await
        .unwrap();
    assert_eq!(text(resp).await, "len=900");
}

/// Replies `len=N` with the number of request body bytes received.
async fn spawn_bulk_backend_counting() -> (SocketAddr, Arc<AtomicUsize>) {
    spawn_backend(|req: Request<Incoming>| async move {
        let mut body = req.into_body();
        let mut len = 0;
        while let Some(frame) = body.frame().await {
            match frame {
                Ok(f) => len += f.data_ref().map_or(0, Bytes::len),
                Err(_) => break,
            }
        }
        ok(full(format!("len={len}")))
    })
    .await
}

// ---- gating and redirects over h2 ---------------------------------------

#[tokio::test]
async fn a_subnet_restricted_route_is_gated_for_h2_clients_by_their_socket_address() {
    let (backend, _) = spawn_backend(describe).await;
    let outside = route(
        None,
        backend,
        RouteOptions {
            allowed_subnet: Some("10.0.0.0/8".parse().unwrap()),
            ..RouteOptions::permissive()
        },
    );
    let proxy = serve(vec![outside]).await;
    let mut client = h2_client(&proxy).await;
    let resp = client
        .send_request(h2_req("GET", "/x", &[], empty()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    let inside = route(
        None,
        backend,
        RouteOptions {
            allowed_subnet: Some("127.0.0.0/8".parse().unwrap()),
            ..RouteOptions::permissive()
        },
    );
    let proxy = serve(vec![inside]).await;
    let mut client = h2_client(&proxy).await;
    let resp = client
        .send_request(h2_req("GET", "/x", &[], empty()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn a_redirect_route_answers_an_h2_client_directly() {
    let redirect = Route::new(HOST, None, true, vec![], false, "redirect", vec![]).with_redirect(
        RedirectTarget {
            url: "https://new.test".to_string(),
            preserve_path: true,
            status: 308,
        },
    );
    let proxy = serve(vec![redirect]).await;
    let mut client = h2_client(&proxy).await;
    let resp = client
        .send_request(h2_req("GET", "/a/b?c=d", &[], empty()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(resp.headers()["location"], "https://new.test/a/b?c=d");
}

// ---- ALPN and protocol edge cases ---------------------------------------

#[tokio::test]
async fn a_client_offering_only_an_unknown_alpn_protocol_is_refused() {
    let (backend, _) = spawn_backend(describe).await;
    let proxy = serve(vec![h1_route(backend)]).await;
    assert!(try_tls_connect(&proxy, &[b"spdy/3"]).await.is_err());
}

#[tokio::test]
async fn a_client_with_no_alpn_still_gets_http11() {
    let (backend, _) = spawn_backend(describe).await;
    let proxy = serve(vec![h1_route(backend)]).await;
    let mut client = h1_client(&proxy, &[]).await;
    let resp = client
        .send_request(h1_req("GET", "/x", &[], empty()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(text(resp).await.starts_with("HTTP/1.1 GET"));
}

#[tokio::test]
async fn an_http11_client_can_use_an_h2c_route_and_a_keep_alive_connection_twice() {
    let (backend, _) = spawn_backend(describe).await;
    let proxy = serve(vec![h2c_route(backend)]).await;
    let mut client = h1_client(&proxy, &[b"http/1.1"]).await;
    for _ in 0..2 {
        let resp = client
            .send_request(h1_req("POST", "/x", &[], full("body")))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(text(resp).await.starts_with("HTTP/2.0 POST"));
    }
}

#[tokio::test]
async fn h2_and_http11_clients_are_served_at_the_same_time() {
    let (backend, _) = spawn_backend(describe).await;
    let proxy = serve(vec![h1_route(backend)]).await;
    let mut h2 = h2_client(&proxy).await;
    let mut h1 = h1_client(&proxy, &[b"http/1.1"]).await;
    let (a, b) = tokio::join!(
        h2.send_request(h2_req("GET", "/x", &[], empty())),
        h1.send_request(h1_req("GET", "/x", &[], empty())),
    );
    assert_eq!(a.unwrap().status(), 200);
    assert_eq!(b.unwrap().status(), 200);
}

/// The plain-HTTP listener is HTTP/1.1 only (no h2c upgrade or prior
/// knowledge). A prior-knowledge h2 client is turned away without disturbing
/// the listener.
#[tokio::test]
async fn the_plain_http_listener_does_not_speak_h2c() {
    let (backend, _) = spawn_backend(describe).await;
    let plain = Route::new(HOST, None, false, vec![], false, "svc", vec![backend])
        .with_options(RouteOptions::permissive());
    let server = Arc::new(ProxyServer::new(
        Arc::new(RouteTable::new(vec![plain])),
        Arc::new(CertStore::new()),
    ));
    let addr = free_addr();
    tokio::spawn(async move { server.serve_http(addr).await.unwrap() });
    wait_for_port(addr).await;

    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .await
        .unwrap();
    let mut reply = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut reply)).await;
    assert!(
        !String::from_utf8_lossy(&reply).contains("200 OK"),
        "prior-knowledge h2 was served on the plain listener"
    );

    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET / HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut reply = String::new();
    s.read_to_string(&mut reply).await.unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
}

#[tokio::test]
async fn an_h2c_route_still_reports_its_protocol_through_version_checks() {
    // Sanity for the harness itself: the dual backend really distinguishes
    // the two upstream protocols.
    let (backend, _) =
        spawn_backend(
            |req: Request<Incoming>| async move { ok(full(format!("{:?}", req.version()))) },
        )
        .await;
    let proxy = serve(vec![
        route(Some("/h1"), backend, RouteOptions::permissive()),
        route(
            Some("/h2"),
            backend,
            RouteOptions {
                http2: true,
                ..RouteOptions::permissive()
            },
        ),
    ])
    .await;
    let mut client = h2_client(&proxy).await;
    for (path, want) in [("/h1/x", Version::HTTP_11), ("/h2/x", Version::HTTP_2)] {
        let resp = client
            .send_request(h2_req("GET", path, &[], empty()))
            .await
            .unwrap();
        assert_eq!(text(resp).await, format!("{want:?}"));
    }
}
