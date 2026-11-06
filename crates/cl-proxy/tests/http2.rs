//! HTTP/2 on the HTTPS listener and h2c to the backend (gRPC): ALPN
//! negotiation, trailers, streaming in both directions, per-route protocol
//! choice, and websockets still working over HTTP/1.1. Self-signed certs
//! generated in-process, no network beyond loopback.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use cl_proxy::{ensure_crypto_provider, CertStore, ProxyServer, Route, RouteOptions, RouteTable};
use cl_raft::CertificateState;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{HeaderMap, Request, Response, Version};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::client::TlsStream;

const HOST: &str = "app.test";

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

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

/// A backend that, like NetBird's server, takes HTTP/1.1 and h2c on one
/// port. It answers every request with the protocol it was spoken to in,
/// the `te` header it saw, and a `grpc-status` trailer.
async fn spawn_dual_backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let svc = service_fn(report);
                let _ = auto::Builder::new(TokioExecutor::new())
                    .serve_connection_with_upgrades(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    addr
}

async fn report(req: Request<Incoming>) -> Result<Response<StreamBody<ReportStream>>, Infallible> {
    let version = format!("{:?}", req.version());
    let te = req
        .headers()
        .get("te")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let path = req.uri().path().to_string();
    let cookies: Vec<_> = req
        .headers()
        .get_all("cookie")
        .iter()
        .map(|v| v.to_str().unwrap_or("?").to_string())
        .collect();
    // Drain the request, keeping its trailers to prove they cross the proxy.
    let mut body = req.into_body();
    let mut req_trailer = "-".to_string();
    while let Some(frame) = body.frame().await {
        if let Some(t) = frame.ok().and_then(|f| f.into_trailers().ok()) {
            req_trailer = t
                .get("x-req-trailer")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
                .to_string();
        }
    }
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", "0".parse().unwrap());
    trailers.insert("x-seen-req-trailer", req_trailer.parse().unwrap());
    let frames: Vec<Result<Frame<Bytes>, BoxErr>> = vec![
        Ok(Frame::data(Bytes::from(format!(
            "{version} te={te} path={path} cookies={}",
            cookies.join("|")
        )))),
        Ok(Frame::trailers(trailers)),
    ];
    Ok(Response::new(StreamBody::new(futures::stream::iter(
        frames,
    ))))
}

type ReportStream = futures::stream::Iter<std::vec::IntoIter<Result<Frame<Bytes>, BoxErr>>>;

struct Proxy {
    https: SocketAddr,
    cert: CertificateState,
}

/// A proxy with `routes` behind a TLS listener holding a cert for `app.test`.
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

fn h2_options() -> RouteOptions {
    RouteOptions {
        http2: true,
        ..RouteOptions::permissive()
    }
}

async fn tls_connect(proxy: &Proxy, alpn: &[&[u8]]) -> TlsStream<TcpStream> {
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
        .unwrap()
}

fn negotiated(tls: &TlsStream<TcpStream>) -> Option<Vec<u8>> {
    tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec)
}

type H2Sender = hyper::client::conn::http2::SendRequest<StreamBody<ReqStream>>;
type ReqStream = tokio_stream_like::Rx;

/// A bare `mpsc` receiver as a body stream, so a test can feed a request body
/// one frame at a time.
mod tokio_stream_like {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use bytes::Bytes;
    use futures::Stream;
    use hyper::body::Frame;

    pub struct Rx(pub tokio::sync::mpsc::Receiver<Frame<Bytes>>);

    impl Stream for Rx {
        type Item = Result<Frame<Bytes>, std::io::Error>;
        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.0.poll_recv(cx).map(|f| f.map(Ok))
        }
    }
}

async fn h2_sender(proxy: &Proxy) -> H2Sender {
    let tls = tls_connect(proxy, &[b"h2", b"http/1.1"]).await;
    assert_eq!(negotiated(&tls).as_deref(), Some(&b"h2"[..]));
    let (sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
            .await
            .unwrap();
    tokio::spawn(conn);
    sender
}

fn request(
    path: &str,
) -> (
    Request<StreamBody<ReqStream>>,
    tokio::sync::mpsc::Sender<Frame<Bytes>>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let req = Request::builder()
        .method("POST")
        .uri(format!("https://{HOST}{path}"))
        .header("te", "trailers")
        .header("content-type", "application/grpc")
        .body(StreamBody::new(tokio_stream_like::Rx(rx)))
        .unwrap();
    (req, tx)
}

#[tokio::test]
async fn the_https_listener_negotiates_h2_and_still_offers_http11() {
    let backend = spawn_dual_backend().await;
    let proxy = serve(vec![route(None, backend, RouteOptions::permissive())]).await;

    let tls = tls_connect(&proxy, &[b"h2", b"http/1.1"]).await;
    assert_eq!(negotiated(&tls).as_deref(), Some(&b"h2"[..]));

    let tls = tls_connect(&proxy, &[b"http/1.1"]).await;
    assert_eq!(negotiated(&tls).as_deref(), Some(&b"http/1.1"[..]));
}

#[tokio::test]
async fn an_h2_client_reaches_a_plain_route_over_http11_upstream() {
    let backend = spawn_dual_backend().await;
    let proxy = serve(vec![route(None, backend, RouteOptions::permissive())]).await;
    let mut sender = h2_sender(&proxy).await;

    let (req, tx) = request("/api/x");
    drop(tx);
    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body);
    assert!(text.starts_with("HTTP/1.1"), "upstream protocol: {text}");
    assert!(text.contains("path=/api/x"), "{text}");
    assert!(text.contains("te=-"), "te must not leak to h1: {text}");
}

#[tokio::test]
async fn an_h2c_route_round_trips_request_and_response_trailers() {
    let backend = spawn_dual_backend().await;
    let proxy = serve(vec![route(None, backend, h2_options())]).await;
    let mut sender = h2_sender(&proxy).await;

    let (req, tx) = request("/management.ManagementService/Login");
    tx.send(Frame::data(Bytes::from_static(b"hello")))
        .await
        .unwrap();
    let mut t = HeaderMap::new();
    t.insert("x-req-trailer", "from-client".parse().unwrap());
    tx.send(Frame::trailers(t)).await.unwrap();
    drop(tx);

    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let collected = resp.into_body().collect().await.unwrap();
    let trailers = collected.trailers().cloned().expect("response trailers");
    assert_eq!(trailers["grpc-status"], "0");
    assert_eq!(trailers["x-seen-req-trailer"], "from-client");
    let text = String::from_utf8_lossy(&collected.to_bytes()).to_string();
    assert!(text.starts_with("HTTP/2.0"), "upstream protocol: {text}");
    assert!(text.contains("te=trailers"), "{text}");
}

#[tokio::test]
async fn routes_on_one_host_and_port_choose_their_upstream_protocol_independently() {
    let backend = spawn_dual_backend().await;
    let proxy = serve(vec![
        route(None, backend, RouteOptions::permissive()),
        route(
            Some("/signalexchange.SignalExchange"),
            backend,
            h2_options(),
        ),
    ])
    .await;
    let mut sender = h2_sender(&proxy).await;

    for (path, want) in [
        ("/signalexchange.SignalExchange/ConnectStream", "HTTP/2.0"),
        ("/api/peers", "HTTP/1.1"),
    ] {
        let (req, tx) = request(path);
        drop(tx);
        let resp = sender.send_request(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with(want), "{path}: {text}");
    }
}

/// A backend that echoes each request chunk back as soon as it arrives and
/// holds its response headers for `header_delay`. Only a proxy that streams
/// in both directions lets the client see an echo before it has finished
/// sending, and only one with no response timeout survives the delay.
async fn spawn_echo_stream_backend(header_delay: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| async move {
                    assert_eq!(req.version(), Version::HTTP_2);
                    tokio::time::sleep(header_delay).await;
                    let (tx, rx) = tokio::sync::mpsc::channel::<Frame<Bytes>>(8);
                    let mut body = req.into_body();
                    tokio::spawn(async move {
                        while let Some(Ok(frame)) = body.frame().await {
                            if tx.send(frame).await.is_err() {
                                return;
                            }
                        }
                    });
                    Ok::<_, Infallible>(Response::new(StreamBody::new(tokio_stream_like::Rx(rx))))
                });
                let _ = auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn a_stream_is_forwarded_both_ways_without_buffering_and_past_the_default_timeout() {
    let backend = spawn_echo_stream_backend(Duration::from_millis(300)).await;
    // 100ms would 504 a plain route; an http2 route with no explicit limit
    // must not apply any, and `0s` must say the same thing explicitly.
    for timeout in [None, Some(Duration::ZERO)] {
        let proxy = serve(vec![route(
            None,
            backend,
            RouteOptions {
                response_timeout: timeout,
                ..h2_options()
            },
        )])
        .await;
        let mut sender = h2_sender(&proxy).await;
        let (req, tx) = request("/signalexchange.SignalExchange/ConnectStream");
        tx.send(Frame::data(Bytes::from_static(b"one")))
            .await
            .unwrap();
        let resp = sender.send_request(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let mut body = resp.into_body();

        // The echo arrives while the request body is still open.
        let first = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("echo of the first chunk never arrived: request body is buffered")
            .unwrap()
            .unwrap();
        assert_eq!(first.into_data().unwrap(), Bytes::from_static(b"one"));

        tx.send(Frame::data(Bytes::from_static(b"two")))
            .await
            .unwrap();
        let second = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("second echo never arrived")
            .unwrap()
            .unwrap();
        assert_eq!(second.into_data().unwrap(), Bytes::from_static(b"two"));
    }
}

#[tokio::test]
async fn an_explicit_response_timeout_still_applies_to_an_h2c_route() {
    let backend = spawn_echo_stream_backend(Duration::from_secs(2)).await;
    let proxy = serve(vec![route(
        None,
        backend,
        RouteOptions {
            response_timeout: Some(Duration::from_millis(200)),
            ..h2_options()
        },
    )])
    .await;
    let mut sender = h2_sender(&proxy).await;
    let (req, tx) = request("/x");
    drop(tx);
    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), 504);
}

/// Websockets are an HTTP/1.1 feature: a client that negotiates http/1.1 on
/// the same listener must still be able to upgrade and relay raw bytes.
#[tokio::test]
async fn a_websocket_upgrade_still_works_over_http11_on_the_same_listener() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf).await.unwrap();
        s.write_all(
            b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
        )
        .await
        .unwrap();
        loop {
            let n = s.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                return;
            }
            s.write_all(&buf[..n]).await.unwrap();
        }
    });

    let proxy = serve(vec![route(None, backend, RouteOptions::permissive())]).await;
    let mut tls = tls_connect(&proxy, &[b"http/1.1"]).await;
    assert_eq!(negotiated(&tls).as_deref(), Some(&b"http/1.1"[..]));
    tls.write_all(
        format!(
            "GET /ws HTTP/1.1\r\nHost: {HOST}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();

    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        tokio::time::timeout(Duration::from_secs(5), tls.read_exact(&mut byte))
            .await
            .expect("no upgrade response")
            .unwrap();
        head.push(byte[0]);
    }
    assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"));

    tls.write_all(b"ping").await.unwrap();
    let mut echo = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(5), tls.read_exact(&mut echo))
        .await
        .expect("no echo through the upgraded connection")
        .unwrap();
    assert_eq!(&echo, b"ping");
}

/// Browsers send one `cookie` header per cookie over h2. A backend (or the
/// sticky lookup) that reads only the first would lose the session cookie, so
/// the proxy joins them, for both upstream protocols.
#[tokio::test]
async fn split_h2_cookie_headers_reach_the_backend_as_one_header() {
    let backend = spawn_dual_backend().await;
    let proxy = serve(vec![
        route(None, backend, RouteOptions::permissive()),
        route(Some("/grpc"), backend, h2_options()),
    ])
    .await;
    let mut sender = h2_sender(&proxy).await;

    for path in ["/login", "/grpc/x"] {
        let (mut req, tx) = request(path);
        drop(tx);
        req.headers_mut()
            .append("cookie", "other=1".parse().unwrap());
        req.headers_mut()
            .append("cookie", "authentik_session=abc".parse().unwrap());
        let resp = sender.send_request(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.ends_with("cookies=other=1; authentik_session=abc"),
            "{path}: {text}"
        );
    }
}
