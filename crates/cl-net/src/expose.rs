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
use tokio::sync::{mpsc, watch, Semaphore};

use crate::error::{NetError, Result};

/// How long a UDP session is kept alive with no traffic in either direction
/// before its resources are reclaimed. UDP has no close handshake, so idle
/// sessions have to be reaped on a timer instead.
const UDP_SESSION_IDLE: Duration = Duration::from_secs(60);

/// How many TCP connections one published port serves at once. Every accepted
/// connection costs two file descriptors and a task, and these ports are open
/// to whoever can reach the node, so without a cap a flood of connections (or a
/// slow-loris) exhausts the daemon's descriptors and takes the whole process
/// down with it. Past the cap a new connection is closed immediately.
const MAX_TCP_CONNECTIONS: usize = 4096;

/// How long to wait for the backend to accept before giving up on a client. A
/// backend on a node that just died would otherwise hold the client (and its
/// permit) for the kernel's multi-minute SYN retry schedule.
const BACKEND_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to back off after an `accept` fails for a reason that is not the
/// listener's fault (out of descriptors, a connection reset before accept).
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// How many UDP client sessions one published port tracks. Each is a socket
/// and a task, and the source address of a UDP packet is trivially forged, so
/// this bounds what a flood of distinct sources can allocate.
const MAX_UDP_SESSIONS: usize = 4096;

/// Datagrams queued per UDP session. Past this the datagram is dropped, which
/// is what an overloaded UDP path does anyway, instead of buffering without
/// bound behind a slow backend.
const UDP_SESSION_QUEUE: usize = 64;

/// How often finished UDP sessions are removed from the table. A session that
/// went idle exits on its own; without this its entry would stay until the same
/// client happened to send again.
const UDP_PRUNE_INTERVAL: Duration = Duration::from_secs(10);

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
///
/// `proxy_protocol` prepends a PROXY v2 header to each backend connection so
/// the backend learns who actually connected. See [`ExposePort::proxy_protocol`]
/// for why that matters and why it is opt in.
///
/// [`ExposePort::proxy_protocol`]: https://docs.rs/cl-config
pub async fn serve_tcp(
    bind: SocketAddr,
    published: u16,
    routes: Routes,
    proxy_protocol: bool,
) -> Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|source| NetError::Io {
            context: format!("bind tcp {bind}"),
            source,
        })?;

    let permits = std::sync::Arc::new(Semaphore::new(MAX_TCP_CONNECTIONS));
    loop {
        let (client, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(source) if is_transient_accept_error(&source) => {
                // Running out of descriptors or a connection that died in the
                // accept queue says nothing about the listener. Returning here
                // would unpublish the port for the retry interval over one
                // bad connection.
                tracing::warn!(%bind, error = %source, "tcp accept failed, retrying");
                tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                continue;
            }
            Err(source) => {
                return Err(NetError::Io {
                    context: format!("accept tcp {bind}"),
                    source,
                })
            }
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            tracing::debug!(%bind, %peer, "too many connections, dropping");
            continue;
        };
        // The local address of the accepted socket, not the listener's: the
        // listener binds 0.0.0.0, and a PROXY header saying the connection
        // was destined for 0.0.0.0 is useless to the backend.
        let local = client.local_addr().ok();
        let mut route = routes.subscribe(published);
        tokio::spawn(async move {
            let header = proxy_protocol
                .then(|| local.map(|local| proxy_v2_header(peer, local)))
                .flatten();
            let _ = forward_tcp(client, &mut route, header).await;
            drop(permit);
        });
    }
}

/// Whether an `accept` error is a passing condition of this one connection or
/// of the process, not a sign the listener is broken.
fn is_transient_accept_error(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    // EMFILE, ENFILE, ENOMEM, ENOBUFS: out of descriptors or memory. They
    // clear as other connections close.
    const TRANSIENT_ERRNO: [i32; 4] = [24, 23, 12, 105];
    matches!(
        error.kind(),
        ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionReset
            | ErrorKind::Interrupted
            | ErrorKind::WouldBlock
            | ErrorKind::TimedOut
    ) || error
        .raw_os_error()
        .is_some_and(|errno| TRANSIENT_ERRNO.contains(&errno))
}

/// Splice one accepted TCP connection to its current backend, aborting if
/// the route changes or there is no backend at all.
///
/// `header`, when present, is written to the backend before any client bytes.
async fn forward_tcp(
    mut client: TcpStream,
    route: &mut watch::Receiver<Option<SocketAddr>>,
    header: Option<Vec<u8>>,
) -> Result<()> {
    let Some(backend_addr) = *route.borrow_and_update() else {
        let _ = client.shutdown().await;
        return Err(NetError::Dns(
            "published port has no running instance".into(),
        ));
    };

    let mut backend =
        match tokio::time::timeout(BACKEND_CONNECT_TIMEOUT, TcpStream::connect(backend_addr))
            .await
            .unwrap_or_else(|_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "backend did not accept in time",
                ))
            }) {
            Ok(stream) => stream,
            Err(source) => {
                let _ = client.shutdown().await;
                return Err(NetError::Io {
                    context: format!("connect backend {backend_addr}"),
                    source,
                });
            }
        };

    // Before anything the client sent: the backend parses this header off the
    // head of the stream, so a single byte of client data ahead of it would
    // make the whole connection unreadable to it.
    if let Some(header) = header {
        if let Err(source) = backend.write_all(&header).await {
            let _ = client.shutdown().await;
            return Err(NetError::Io {
                context: format!("write proxy protocol header to {backend_addr}"),
                source,
            });
        }
    }

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

/// The 12 byte signature every PROXY v2 header starts with. Chosen by the
/// spec to be something no real protocol would ever begin with, so a backend
/// can tell a header from ordinary traffic.
const PROXY_V2_SIGNATURE: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];

/// Encode a PROXY protocol v2 header announcing `client` as the real peer of
/// a connection that arrived for `local`.
///
/// v2 (binary) rather than v1 (text) because it is what current servers
/// implement and it cannot be confused with a real request line. The address
/// family follows the pair: both are always the same family here, since
/// `local` is the local end of the very socket `client` connected to.
///
/// Deliberately no TLVs. They are optional in the spec, nothing here has
/// anything to put in one, and every byte of this header is parsed by the
/// backend before it will talk to us.
fn proxy_v2_header(client: SocketAddr, local: SocketAddr) -> Vec<u8> {
    let mut header = Vec::with_capacity(28);
    header.extend_from_slice(&PROXY_V2_SIGNATURE);
    // Version 2 (high nibble), command PROXY (low nibble) — as opposed to
    // LOCAL, which would tell the backend to ignore the addresses and use
    // the real socket, i.e. exactly the behaviour we are fixing.
    header.push(0x21);

    match (client, local) {
        (SocketAddr::V4(client), SocketAddr::V4(local)) => {
            header.push(0x11); // AF_INET + STREAM
            header.extend_from_slice(&12u16.to_be_bytes());
            header.extend_from_slice(&client.ip().octets());
            header.extend_from_slice(&local.ip().octets());
            header.extend_from_slice(&client.port().to_be_bytes());
            header.extend_from_slice(&local.port().to_be_bytes());
        }
        (SocketAddr::V6(client), SocketAddr::V6(local)) => {
            header.push(0x21); // AF_INET6 + STREAM
            header.extend_from_slice(&36u16.to_be_bytes());
            header.extend_from_slice(&client.ip().octets());
            header.extend_from_slice(&local.ip().octets());
            header.extend_from_slice(&client.port().to_be_bytes());
            header.extend_from_slice(&local.port().to_be_bytes());
        }
        // Mixed families cannot be described by one address block. Rather
        // than invent an address, fall back to UNSPEC with no body: a
        // conforming backend accepts the connection and uses the real socket
        // addresses, which is no worse than not sending a header at all.
        _ => {
            header.push(0x00); // AF_UNSPEC
            header.extend_from_slice(&0u16.to_be_bytes());
        }
    }
    header
}

/// Serve a published UDP port. Each distinct client address gets its own
/// session bound to the backend that was current when the session started;
/// the session is torn down (and a new one started fresh) if the route
/// changes or the session goes idle.
pub async fn serve_udp(bind: SocketAddr, published: u16, routes: Routes) -> Result<()> {
    let socket =
        std::sync::Arc::new(UdpSocket::bind(bind).await.map_err(|source| NetError::Io {
            context: format!("bind udp {bind}"),
            source,
        })?);

    let mut sessions: BTreeMap<SocketAddr, mpsc::Sender<Vec<u8>>> = BTreeMap::new();
    let mut buf = vec![0u8; 65_507];
    let mut prune = tokio::time::interval(UDP_PRUNE_INTERVAL);

    loop {
        let (len, client_addr) = tokio::select! {
            _ = prune.tick() => {
                sessions.retain(|_, tx| !tx.is_closed());
                continue;
            }
            received = socket.recv_from(&mut buf) => match received {
                Ok(received) => received,
                Err(source) if is_transient_accept_error(&source) => {
                    tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                    continue;
                }
                Err(source) => {
                    return Err(NetError::Io {
                        context: format!("recv udp {bind}"),
                        source,
                    })
                }
            },
        };
        let mut datagram = buf[..len].to_vec();

        if let Some(tx) = sessions.get(&client_addr) {
            match tx.try_send(datagram) {
                Ok(()) => continue,
                // A backend slower than the client: drop, like any UDP hop.
                Err(mpsc::error::TrySendError::Full(_)) => continue,
                // Session task exited (idle timeout or route change); fall
                // through and start a fresh one below.
                Err(mpsc::error::TrySendError::Closed(returned)) => {
                    sessions.remove(&client_addr);
                    datagram = returned;
                }
            }
        }

        if sessions.len() >= MAX_UDP_SESSIONS {
            sessions.retain(|_, tx| !tx.is_closed());
            if sessions.len() >= MAX_UDP_SESSIONS {
                continue;
            }
        }

        let Some(backend_addr) = *routes.subscribe(published).borrow() else {
            continue; // no instance running: silently drop, same as an unroutable packet
        };

        let (tx, rx) = mpsc::channel(UDP_SESSION_QUEUE);
        let _ = tx.try_send(datagram);
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
    mut inbound: mpsc::Receiver<Vec<u8>>,
) {
    // In the backend's own address family: an IPv4 socket cannot be connected
    // to an IPv6 backend, nor the reverse.
    let local: SocketAddr = if backend_addr.is_ipv4() {
        ([0, 0, 0, 0], 0).into()
    } else {
        ([0u16; 8], 0).into()
    };
    let Ok(backend) = UdpSocket::bind(local).await else {
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

    #[test]
    fn running_out_of_descriptors_is_a_transient_accept_error() {
        assert!(is_transient_accept_error(
            &std::io::Error::from_raw_os_error(24)
        ));
        assert!(is_transient_accept_error(&std::io::Error::from(
            std::io::ErrorKind::ConnectionAborted
        )));
        // A closed or invalid listener is not coming back by retrying.
        assert!(!is_transient_accept_error(
            &std::io::Error::from_raw_os_error(9)
        ));
        assert!(!is_transient_accept_error(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
    }

    #[tokio::test]
    async fn a_session_towards_an_ipv6_backend_binds_an_ipv6_socket() {
        // Skipped quietly where the host has no IPv6 loopback.
        let Ok(backend) = UdpSocket::bind("[::1]:0").await else {
            return;
        };
        let backend_addr = backend.local_addr().unwrap();
        let published = std::sync::Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let routes = Routes::new();
        let (tx, rx) = mpsc::channel(UDP_SESSION_QUEUE);
        tx.send(b"ping".to_vec()).await.unwrap();
        tokio::spawn(run_udp_session(
            published,
            client.local_addr().unwrap(),
            backend_addr,
            routes.subscribe(1),
            rx,
        ));
        let mut buf = [0u8; 16];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), backend.recv_from(&mut buf))
            .await
            .expect("the datagram reaches the v6 backend")
            .unwrap();
        assert_eq!(&buf[..len], b"ping");
    }

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
                let Ok((mut conn, _)) = listener.accept().await else {
                    return;
                };
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
        tokio::spawn(serve_tcp(bind, bind.port(), routes.clone(), false));
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

        tokio::spawn(serve_tcp(published, published.port(), routes, false));
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

        tokio::spawn(serve_tcp(
            published,
            published.port(),
            routes.clone(),
            false,
        ));
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

    /// A backend that reports the first bytes it received, so a test can
    /// assert on the PROXY header rather than on an echo.
    async fn recording_backend() -> (SocketAddr, std::sync::Arc<Mutex<Vec<u8>>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        tokio::spawn(async move {
            let Ok((mut conn, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 256];
            if let Ok(n) = conn.read(&mut buf).await {
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        (addr, seen)
    }

    #[test]
    fn a_v4_header_carries_both_ends_of_the_connection() {
        let header = proxy_v2_header(
            "203.0.113.7:54321".parse().unwrap(),
            "10.244.1.1:25".parse().unwrap(),
        );
        assert_eq!(&header[..12], &PROXY_V2_SIGNATURE);
        assert_eq!(header[12], 0x21, "version 2, command PROXY");
        assert_eq!(header[13], 0x11, "AF_INET over STREAM");
        assert_eq!(&header[14..16], &12u16.to_be_bytes(), "v4 body is 12 bytes");
        assert_eq!(&header[16..20], &[203, 0, 113, 7], "the real client");
        assert_eq!(
            &header[20..24],
            &[10, 244, 1, 1],
            "the address it connected to"
        );
        assert_eq!(&header[24..26], &54321u16.to_be_bytes());
        assert_eq!(&header[26..28], &25u16.to_be_bytes());
        assert_eq!(header.len(), 28);
    }

    #[test]
    fn a_v6_header_is_well_formed() {
        let header = proxy_v2_header(
            "[2001:db8::1]:443".parse().unwrap(),
            "[2001:db8::2]:25".parse().unwrap(),
        );
        assert_eq!(header[13], 0x21, "AF_INET6 over STREAM");
        assert_eq!(&header[14..16], &36u16.to_be_bytes());
        assert_eq!(header.len(), 52);
    }

    /// One end v4 and the other v6 cannot be described by a single address
    /// block, and inventing an address would be worse than saying nothing.
    #[test]
    fn a_mixed_family_pair_falls_back_to_unspec() {
        let header = proxy_v2_header(
            "203.0.113.7:1".parse().unwrap(),
            "[2001:db8::2]:25".parse().unwrap(),
        );
        assert_eq!(header[13], 0x00, "AF_UNSPEC");
        assert_eq!(&header[14..16], &0u16.to_be_bytes(), "no body");
        assert_eq!(header.len(), 16);
    }

    #[tokio::test]
    async fn the_backend_is_told_the_real_client_address() {
        let (backend, seen) = recording_backend().await;
        let published = free_addr().await;
        let routes = Routes::new();
        routes.set(published.port(), Some(backend));

        tokio::spawn(serve_tcp(published, published.port(), routes, true));
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut client = TcpStream::connect(published).await.unwrap();
        let client_port = client.local_addr().unwrap().port();
        client.write_all(b"HELO").await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;

        let seen = seen.lock().unwrap().clone();
        assert_eq!(&seen[..12], &PROXY_V2_SIGNATURE, "header must come first");
        assert_eq!(&seen[16..20], &[127, 0, 0, 1]);
        assert_eq!(
            u16::from_be_bytes([seen[24], seen[25]]),
            client_port,
            "the client's own port, not the relay's",
        );
        assert_eq!(&seen[28..32], b"HELO", "client data follows the header");
    }

    /// The default must stay byte-for-byte what it always was: a backend that
    /// does not speak PROXY protocol sees a malformed request if a header
    /// ever appears in front of its traffic.
    #[tokio::test]
    async fn no_header_is_sent_unless_it_was_asked_for() {
        let (backend, seen) = recording_backend().await;
        let published = free_addr().await;
        let routes = Routes::new();
        routes.set(published.port(), Some(backend));

        tokio::spawn(serve_tcp(published, published.port(), routes, false));
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut client = TcpStream::connect(published).await.unwrap();
        client.write_all(b"HELO").await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;

        assert_eq!(&seen.lock().unwrap().clone(), b"HELO");
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
