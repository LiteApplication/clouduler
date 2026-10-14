//! How the proxy's pooled client opens a connection to a backend.
//!
//! `HttpConnector` would dial the backend address from this host, which only
//! works for a container this node runs: a container on another node cannot
//! be reached from here over the overlay at all (see `cl_net::relay`). This
//! connector hands the dial to a `cl_net::relay::Dialer` instead, which goes
//! through the owning node's relay for a remote container and connects
//! directly to anything else - a local container, the dashboard on loopback,
//! a static backend outside the cluster.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use cl_net::relay::Dialer;
use http::Uri;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

#[derive(Clone)]
pub(crate) struct BackendConnector {
    dialer: Dialer,
    /// Bounds the whole dial, a relayed one included.
    timeout: Duration,
}

impl BackendConnector {
    pub(crate) fn new(dialer: Dialer, timeout: Duration) -> Self {
        Self { dialer, timeout }
    }
}

impl tower_service::Service<Uri> for BackendConnector {
    type Response = TokioIo<TcpStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let dialer = self.dialer.clone();
        let timeout = self.timeout;
        Box::pin(async move {
            let addr = backend_addr(&uri).await?;
            let stream = dialer.connect_tcp(addr, timeout).await?;
            Ok(TokioIo::new(stream))
        })
    }
}

/// The socket address a backend uri names. Every backend the routing table
/// holds is already an address (`server::backend_uri` builds the uri from
/// one), so the lookup is only a fallback for a uri that somehow names a host.
async fn backend_addr(uri: &Uri) -> io::Result<SocketAddr> {
    let host = uri.host().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("backend uri {uri} has no host"),
        )
    })?;
    let port = uri.port_u16().unwrap_or(80);
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    tokio::net::lookup_host((bare, port))
        .await?
        .next()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("backend host {host} resolves to nothing"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_backend_uri_names_its_socket_address() {
        let v4: Uri = "http://10.244.3.8:8080/x?y".parse().unwrap();
        assert_eq!(
            backend_addr(&v4).await.unwrap(),
            "10.244.3.8:8080".parse().unwrap()
        );
        let v6: Uri = "http://[fd00::8]:80/".parse().unwrap();
        assert_eq!(
            backend_addr(&v6).await.unwrap(),
            "[fd00::8]:80".parse().unwrap()
        );
        let default_port: Uri = "http://10.244.3.8/".parse().unwrap();
        assert_eq!(backend_addr(&default_port).await.unwrap().port(), 80);
    }
}
