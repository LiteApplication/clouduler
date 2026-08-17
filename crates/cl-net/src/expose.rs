//! Non-HTTP published ports (`x-clouduler.expose`, e.g. gitlab's ssh on 22).
//!
//! A published port is bound on every node. Whichever node currently runs
//! the instance is reached over the overlay by forwarding in userspace: a
//! straightforward `copy_bidirectional` splice for TCP, and a small
//! per-client session table for UDP. No extra kernel plumbing (no DNAT
//! rules to keep in sync with placement) at the cost of one extra hop
//! through this process's memory.
//!
//! Routing is driven by [`Routes`], a table the reconciler updates whenever
//! placement changes. Forwarders watch their route rather than resolving it
//! once per connection, so a service moving to another node while a
//! connection is open closes that connection immediately instead of hanging
//! against a backend that's no longer there.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::watch;

use crate::error::{NetError, Result};

/// How long a UDP session is kept alive with no traffic in either direction
/// before its resources are reclaimed. UDP has no close handshake, so idle
/// sessions have to be reaped on a timer instead.
const UDP_SESSION_IDLE: Duration = Duration::from_secs(60);

/// The current backend for every published port this node forwards. Shared
/// between the reconciler (which calls `set` as placement changes) and the
/// forwarder tasks (which call `subscribe` once at accept time and then
/// watch for changes for the life of the connection).
#[derive(Clone, Default)]
pub struct Routes {
    inner: std::sync::Arc<Mutex<BTreeMap<u16, watch::Sender<Option<SocketAddr>>>>>,
}

impl Routes {
    pub fn new() -> Self {
        Self::default()
    }

    /// Watch channel for a published port's current backend. Creates the
    /// channel (initially routeless) the first time a port is referenced,
    /// either by `set` or `subscribe`, whichever happens first.
    fn channel(&self, published: u16) -> watch::Sender<Option<SocketAddr>> {
        let mut inner = self.inner.lock().expect("routes lock poisoned");
        inner
            .entry(published)
            .or_insert_with(|| watch::channel(None).0)
            .clone()
    }

    /// Subscribe to a published port's backend. The receiver always starts
    /// with the current value, so a forwarder that just accepted a
    /// connection sees the right backend immediately.
    pub fn subscribe(&self, published: u16) -> watch::Receiver<Option<SocketAddr>> {
        self.channel(published).subscribe()
    }

    /// Point a published port at a backend, or clear it with `None` when the
    /// instance isn't running anywhere right now. Any forwarder currently
    /// serving that port notices on its next watch poll.
    pub fn set(&self, published: u16, backend: Option<SocketAddr>) {
        // send_if_modified avoids waking every watcher on a reconcile pass
        // that didn't actually change anything.
        self.channel(published).send_if_modified(|current| {
            if *current == backend {
                false
            } else {
                *current = backend;
                true
            }
        });
    }
}

/// Serve a published TCP port, forwarding accepted connections to whatever
/// `routes` currently says the backend is. Runs until the listener errors;
/// callers spawn this as a background task per published port.
pub async fn serve_tcp(bind: SocketAddr, published: u16, routes: Routes) -> Result<()> {
    let listener = TcpListener::bind(bind).await.map_err(|source| NetError::Io {
        context: format!("bind tcp {bind}"),
        source,
    })?;

    loop {
        let (client, _peer) = listener.accept().await.map_err(|source| NetError::Io {
            context: format!("accept tcp {bind}"),
            source,
        })?;
        let mut route = routes.subscribe(published);
        tokio::spawn(async move {
            let _ = forward_tcp(client, &mut route).await;
        });
    }
}

/// Splice one accepted TCP connection to its current backend, aborting if
/// the route changes or there is no backend at all.
async fn forward_tcp(mut client: TcpStream, route: &mut watch::Receiver<Option<SocketAddr>>) -> Result<()> {
    let Some(backend_addr) = *route.borrow_and_update() else {
        let _ = client.shutdown().await;
        return Err(NetError::Dns("published port has no running instance".into()));
    };

    let mut backend = match TcpStream::connect(backend_addr).await {
        Ok(stream) => stream,
        Err(source) => {
            let _ = client.shutdown().await;
            return Err(NetError::Io {
                context: format!("connect backend {backend_addr}"),
                source,
            });
        }
    };

    tokio::select! {
        result = tokio::io::copy_bidirectional(&mut client, &mut backend) => {
            result.map(|_| ()).map_err(|source| NetError::Io {
                context: "forwarded tcp connection".into(),
                source,
            })
        }
        // The instance moved (or was unassigned): the old backend address is
        // no longer meaningful, so drop both sides now rather than let the
        // client sit on a connection nobody is serving.
        _ = route.changed() => Ok(()),
    }
}

/// Serve a published UDP port. Each distinct client address gets its own
/// session bound to the backend that was current when the session started;
/// the session is torn down (and a new one started fresh) if the route
/// changes or the session goes idle.
pub async fn serve_udp(bind: SocketAddr, published: u16, routes: Routes) -> Result<()> {
    let socket = std::sync::Arc::new(UdpSocket::bind(bind).await.map_err(|source| NetError::Io {
        context: format!("bind udp {bind}"),
        source,
    })?);

    let mut sessions: BTreeMap<SocketAddr, tokio::sync::mpsc::UnboundedSender<Vec<u8>>> = BTreeMap::new();
    let mut buf = vec![0u8; 65_507];

    loop {
        let (len, client_addr) = socket.recv_from(&mut buf).await.map_err(|source| NetError::Io {
            context: format!("recv udp {bind}"),
            source,
        })?;
        let mut datagram = buf[..len].to_vec();

        if let Some(tx) = sessions.get(&client_addr) {
            match tx.send(datagram) {
                Ok(()) => continue,
                // Session task exited (idle timeout or route change); fall
                // through and start a fresh one below.
                Err(e) => {
                    sessions.remove(&client_addr);
                    datagram = e.0;
                }
            }
        }

        let Some(backend_addr) = *routes.subscribe(published).borrow() else {
            continue; // no instance running: silently drop, same as an unroutable packet
        };

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = tx.send(datagram);
        sessions.insert(client_addr, tx);

        tokio::spawn(run_udp_session(
            socket.clone(),
            client_addr,
            backend_addr,
            routes.subscribe(published),
            rx,
        ));
    }
}

/// One client's UDP session: a dedicated socket connected to the backend,
/// pumping datagrams in both directions until idle, the route changes, or
/// the backend goes away.
async fn run_udp_session(
    published: std::sync::Arc<UdpSocket>,
    client_addr: SocketAddr,
    backend_addr: SocketAddr,
    mut route: watch::Receiver<Option<SocketAddr>>,
    mut inbound: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let Ok(backend) = UdpSocket::bind(("0.0.0.0", 0)).await else {
        return;
    };
    if backend.connect(backend_addr).await.is_err() {
        return;
    }

    let mut buf = vec![0u8; 65_507];
    loop {
        tokio::select! {
            datagram = inbound.recv() => {
                match datagram {
                    Some(datagram) => {
                        if backend.send(&datagram).await.is_err() {
                            return;
                        }
                    }
                    None => return, // no more traffic queued for this session
                }
            }
            result = backend.recv(&mut buf) => {
                match result {
                    Ok(len) => {
                        let _ = published.send_to(&buf[..len], client_addr).await;
                    }
                    Err(_) => return,
                }
            }
            _ = route.changed() => return, // instance moved: drop the session
            _ = tokio::time::sleep(UDP_SESSION_IDLE) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    async fn free_addr() -> SocketAddr {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        addr
    }

    async fn echo_backend() -> SocketAddr {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut conn, _)) = listener.accept().await else { return };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    loop {
                        match conn.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                if conn.write_all(&buf[..n]).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn forwards_tcp_traffic_to_the_current_backend() {
        let backend = echo_backend().await;
        let published = free_addr().await;
        let routes = Routes::new();
        routes.set(published.port(), Some(backend));

        let bind = published;
        tokio::spawn(serve_tcp(bind, bind.port(), routes.clone()));
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut client = TcpStream::connect(bind).await.unwrap();
        client.write_all(b"hello overlay").await.unwrap();
        let mut buf = [0u8; 32];
        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello overlay");
    }

    #[tokio::test]
    async fn connection_with_no_route_is_closed_not_hung() {
        let published = free_addr().await;
        let routes = Routes::new(); // no backend set for this port

        tokio::spawn(serve_tcp(published, published.port(), routes));
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut client = TcpStream::connect(published).await.unwrap();
        let mut buf = [0u8; 32];
        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(n, 0, "connection should be closed immediately, not hang");
    }

    #[tokio::test]
    async fn moving_the_instance_mid_connection_closes_it_cleanly() {
        let backend = echo_backend().await;
        let published = free_addr().await;
        let routes = Routes::new();
        routes.set(published.port(), Some(backend));

        tokio::spawn(serve_tcp(published, published.port(), routes.clone()));
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut client = TcpStream::connect(published).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 32];
        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ping");

        // Instance "moves": point the route at a different (nonexistent)
        // backend. The open connection must be torn down, not left hanging.
        let moved_backend = free_addr().await;
        routes.set(published.port(), Some(moved_backend));

        let mut buf = [0u8; 32];
        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(n, 0, "connection should close when the instance moves");
    }

    #[tokio::test]
    async fn routes_are_idempotent_when_unchanged() {
        let routes = Routes::new();
        let backend: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        routes.set(2222, Some(backend));
        let mut rx = routes.subscribe(2222);
        assert!(rx.borrow_and_update().is_some());
        // Setting the same value again must not mark the channel changed.
        routes.set(2222, Some(backend));
        assert!(!rx.has_changed().unwrap());
    }
}
