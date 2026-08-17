//! TLS listener test: SNI picks the right certificate out of two loaded ones.
//! No real network, no real CA; self-signed certs generated in-process.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;

use cl_proxy::{ensure_crypto_provider, CertStore, ProxyServer, RouteTable};
use cl_raft::CertificateState;

fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
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

fn make_cert(name: &str) -> CertificateState {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
    CertificateState {
        fqdn: name.to_string(),
        chain_pem: cert.pem(),
        key_pem: signing_key.serialize_pem(),
        not_after: 0,
    }
}

/// Connects with the given SNI name and returns the leaf cert's subject
/// common name isn't easily accessible without more parsing, so instead we
/// just assert the handshake succeeds with a client trusting each individual
/// cert as its own root, per name.
async fn handshake_succeeds_for(addr: SocketAddr, sni: &str, trust_pem: &str) -> bool {
    let mut roots = rustls::RootCertStore::empty();
    let (_, pem) = x509_parser::pem::parse_x509_pem(trust_pem.as_bytes()).unwrap();
    roots
        .add(rustls::pki_types::CertificateDer::from(pem.contents))
        .unwrap();
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));

    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let name = ServerName::try_from(sni.to_string()).unwrap();
    connector.connect(name, stream).await.is_ok()
}

#[tokio::test]
async fn sni_selects_matching_certificate_between_two_loaded() {
    ensure_crypto_provider();

    let cert_a = make_cert("a.test");
    let cert_b = make_cert("b.test");
    let certs = Arc::new(CertStore::new());
    certs.load(&[cert_a.clone(), cert_b.clone()]);

    let routes = Arc::new(RouteTable::new(vec![]));
    let server = Arc::new(ProxyServer::new(routes, certs));

    let addr = free_addr();
    let s = server.clone();
    tokio::spawn(async move { s.serve_https(addr).await.unwrap() });
    wait_for_port(addr).await;

    // Each handshake trusts only its own cert as a root: it must only
    // succeed when the server actually resolves that host's certificate for
    // that SNI name, proving SNI-based selection works both ways.
    assert!(handshake_succeeds_for(addr, "a.test", &cert_a.chain_pem).await);
    assert!(handshake_succeeds_for(addr, "b.test", &cert_b.chain_pem).await);

    // Cross-checking with the wrong trust root must fail: b's cert isn't
    // trusted by a client that only trusts a's root, so if the server ever
    // served the wrong cert for a given SNI this would spuriously pass.
    assert!(!handshake_succeeds_for(addr, "a.test", &cert_b.chain_pem).await);
}
