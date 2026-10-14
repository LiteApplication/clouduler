//! Reaching a container on another node through the node that runs it.
//!
//! Every node's bridge for a stack network holds the same anycast gateway
//! address and MAC (`vxlan::gateway_mac`), which is what keeps a container's
//! gateway on its own node. The cost is that a node's *host* cannot open a
//! connection to a container on another node over the overlay: the host's
//! packets carry the shared gateway address as their source, the container
//! answers that address, and its own node's bridge takes the answer. That
//! node holds no socket for the flow and resets it (and
//! `Overlay::keep_gateway_frames_local` keeps the host's frames off the
//! tunnel in the first place).
//!
//! So every connection the host itself makes to a container - the reverse
//! proxy's backends and health probes, the `x-clouduler.expose` forwarders -
//! goes through the node that runs the container instead. The dialing node
//! opens a TCP connection to that node's relay listener on the internal
//! network, both sides prove they hold the cluster key
//! (`cl_secret::stream_auth`, label `relay`), and the owner dials the
//! container locally, where the anycast gateway is the right one, then
//! splices the two connections together. A container on the dialing node
//! itself is dialed directly, exactly as before.
//!
//! After the handshake the client names its target:
//!
//! ```text
//! client -> server   kind (1 tcp, 2 udp) || family (4, 6) || address || port (u16 be)
//! server -> client   0                            the target is connected
//!                    1 || len (u16 be) || reason  it is not, and why
//! ```
//!
//! and from then on the stream is the target's: raw bytes in both directions
//! for TCP, and for UDP one `len (u16 be) || payload` frame per datagram.
//!
//! The listener only dials addresses inside the pools it is handed (the
//! container overlay), and always dials them directly, never onward through
//! another relay: two nodes briefly disagreeing about where a container runs
//! costs one failed connection, never a loop.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use cl_raft::KeyWatch;
use cl_secret::stream_auth;
use ipnet::IpNet;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Semaphore;
use tracing::{debug, warn};

/// Scopes the stream handshake to this protocol, so a proof captured on the
/// raft or drive channel cannot open this one.
pub const AUTH_LABEL: &str = "relay";

/// The relay listens on the node's internal address at its rpc port + 2, next
/// to the drive transport at + 1 (`cl_drive::transport::drive_port`).
pub fn relay_port(rpc_port: u16) -> u16 {
    rpc_port.wrapping_add(2)
}

/// Unauthenticated connections the listener holds at once. Real peers
/// handshake in milliseconds; past the cap a new connection is dropped
/// rather than queued, since a queued unauthenticated connection is exactly
/// the resource being protected.
const MAX_PENDING_HANDSHAKES: usize = 64;

/// Relayed connections the listener serves at once. Each costs two
/// descriptors and a task.
const MAX_RELAYED: usize = 4096;

/// How long an authenticated client has to name its target.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the owning node waits for the container to accept. Matches the
/// expose forwarders' own backend timeout.
const TARGET_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a relayed UDP flow may go without a datagram either way before
/// the owning node drops it. Longer than the forwarders' own session idle
/// (`expose::UDP_SESSION_IDLE`), which closes the stream first in the normal
/// case; this only reclaims a flow whose client node vanished without
/// closing it.
const UDP_RELAY_IDLE: Duration = Duration::from_secs(180);

/// Largest UDP payload, and so the largest datagram frame.
const MAX_DATAGRAM: usize = 65_507;

/// Cap on the reason text an error status carries.
const MAX_REASON: usize = 512;

const STATUS_OK: u8 = 0;
const STATUS_ERROR: u8 = 1;

/// What the client wants spliced to its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Tcp = 1,
    Udp = 2,
}

/// Dials containers on the host's behalf: directly when this node runs them,
/// through the owning node's relay otherwise. Cheap to clone; every clone
/// shares one relay table, which the daemon refreshes from placement.
#[derive(Clone, Default)]
pub struct Dialer {
    inner: Arc<DialerInner>,
}

#[derive(Default)]
struct DialerInner {
    /// `None` only for a dialer that never relays (tests, tools).
    keys: Option<KeyWatch>,
    /// Container address -> the relay endpoint of the node running it, for
    /// containers this node does not run. Anything absent is dialed directly.
    relays: RwLock<Arc<HashMap<IpAddr, SocketAddr>>>,
}

impl Dialer {
    /// A dialer that always connects directly, for callers with no cluster
    /// around them.
    pub fn direct() -> Self {
        Self::default()
    }

    /// A dialer that authenticates to relays with the live cluster key.
    pub fn new(keys: KeyWatch) -> Self {
        Self {
            inner: Arc::new(DialerInner {
                keys: Some(keys),
                relays: RwLock::default(),
            }),
        }
    }

    /// Replace the whole relay table: container address -> relay endpoint of
    /// its node, for every container this node does not run itself.
    pub fn set_relays(&self, relays: HashMap<IpAddr, SocketAddr>) {
        let mut table = self.inner.relays.write().unwrap_or_else(|e| e.into_inner());
        if **table != relays {
            *table = Arc::new(relays);
        }
    }

    /// The relay to go through for `ip`, or `None` to dial it directly.
    pub fn relay_for(&self, ip: IpAddr) -> Option<SocketAddr> {
        self.inner
            .relays
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ip)
            .copied()
    }

    /// A TCP connection to `target`, through its node's relay when another
    /// node runs it. `timeout` bounds the whole dial, relay included.
    pub async fn connect_tcp(
        &self,
        target: SocketAddr,
        timeout: Duration,
    ) -> io::Result<TcpStream> {
        match self.relay_for(target.ip()) {
            None => tokio::time::timeout(timeout, TcpStream::connect(target))
                .await
                .unwrap_or_else(|_| {
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("connecting to {target} timed out"),
                    ))
                }),
            Some(relay) => self.through(relay, Kind::Tcp, target, timeout).await,
        }
    }

    /// A stream of `len || payload` datagram frames to the UDP `target`
    /// through `relay` (see `relay_for`), for a forwarder whose backend runs
    /// on another node. Read and write frames with `read_datagram` and
    /// `write_datagram`.
    pub async fn connect_udp(
        &self,
        relay: SocketAddr,
        target: SocketAddr,
        timeout: Duration,
    ) -> io::Result<TcpStream> {
        self.through(relay, Kind::Udp, target, timeout).await
    }

    async fn through(
        &self,
        relay: SocketAddr,
        kind: Kind,
        target: SocketAddr,
        timeout: Duration,
    ) -> io::Result<TcpStream> {
        tokio::time::timeout(timeout, self.open(relay, kind, target))
            .await
            .unwrap_or_else(|_| {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("reaching {target} through its node's relay at {relay} timed out"),
                ))
            })
    }

    async fn open(
        &self,
        relay: SocketAddr,
        kind: Kind,
        target: SocketAddr,
    ) -> io::Result<TcpStream> {
        // Cloned out of the watch at once: a borrow must not be held across
        // an await.
        let key = self
            .inner
            .keys
            .as_ref()
            .and_then(|keys| keys.borrow().clone())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{target} runs on another node, and this node holds no cluster key to authenticate to its relay at {relay}"
                    ),
                )
            })?;
        let mut stream = TcpStream::connect(relay).await.map_err(|e| {
            context(
                e,
                format!("connecting to the relay at {relay} for {target}"),
            )
        })?;
        let _ = stream.set_nodelay(true);
        stream_auth::connect(&mut stream, &key, AUTH_LABEL)
            .await
            .map_err(|e| {
                io::Error::other(format!("authenticating to the relay at {relay}: {e}"))
            })?;
        stream.write_all(&encode_request(kind, target)).await?;
        stream.flush().await?;
        read_status(&mut stream)
            .await
            .map_err(|e| context(e, format!("the relay at {relay} could not reach {target}")))?;
        Ok(stream)
    }
}

/// `e` with `what` in front of its message, keeping its kind.
fn context(e: io::Error, what: String) -> io::Error {
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn encode_request(kind: Kind, target: SocketAddr) -> Vec<u8> {
    let mut out = Vec::with_capacity(20);
    out.push(kind as u8);
    match target.ip() {
        IpAddr::V4(ip) => {
            out.push(4);
            out.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            out.push(6);
            out.extend_from_slice(&ip.octets());
        }
    }
    out.extend_from_slice(&target.port().to_be_bytes());
    out
}

async fn read_request<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<(Kind, SocketAddr)> {
    let kind = match stream.read_u8().await? {
        1 => Kind::Tcp,
        2 => Kind::Udp,
        other => return Err(invalid(format!("unknown relay kind {other}"))),
    };
    let ip = match stream.read_u8().await? {
        4 => {
            let mut octets = [0u8; 4];
            stream.read_exact(&mut octets).await?;
            IpAddr::from(octets)
        }
        6 => {
            let mut octets = [0u8; 16];
            stream.read_exact(&mut octets).await?;
            IpAddr::from(octets)
        }
        other => return Err(invalid(format!("unknown address family {other}"))),
    };
    let port = stream.read_u16().await?;
    Ok((kind, SocketAddr::new(ip, port)))
}

async fn write_status<S: AsyncWrite + Unpin>(
    stream: &mut S,
    result: Result<(), String>,
) -> io::Result<()> {
    match result {
        Ok(()) => stream.write_all(&[STATUS_OK]).await?,
        Err(reason) => {
            let mut end = reason.len().min(MAX_REASON);
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            let reason = &reason.as_bytes()[..end];
            let mut out = Vec::with_capacity(3 + reason.len());
            out.push(STATUS_ERROR);
            out.extend_from_slice(&(reason.len() as u16).to_be_bytes());
            out.extend_from_slice(reason);
            stream.write_all(&out).await?;
        }
    }
    stream.flush().await
}

async fn read_status<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<()> {
    match stream.read_u8().await? {
        STATUS_OK => Ok(()),
        STATUS_ERROR => {
            let len = stream.read_u16().await? as usize;
            let mut reason = vec![0u8; len];
            stream.read_exact(&mut reason).await?;
            Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                String::from_utf8_lossy(&reason).into_owned(),
            ))
        }
        other => Err(invalid(format!("unknown relay status {other}"))),
    }
}

/// Write one datagram as a `len (u16 be) || payload` frame.
pub async fn write_datagram<S: AsyncWrite + Unpin>(
    stream: &mut S,
    datagram: &[u8],
) -> io::Result<()> {
    let len = u16::try_from(datagram.len()).map_err(|_| {
        invalid(format!(
            "a {} byte datagram does not fit a frame",
            datagram.len()
        ))
    })?;
    let mut frame = Vec::with_capacity(2 + datagram.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(datagram);
    stream.write_all(&frame).await
}

/// Read one datagram frame. `None` on a clean end of stream between frames.
/// Not cancel safe: keep it on a task of its own rather than in a `select!`
/// branch that can lose to another one mid frame.
pub async fn read_datagram<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<Option<Vec<u8>>> {
    let len = match stream.read_u16().await {
        Ok(len) => len as usize,
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut datagram = vec![0u8; len];
    stream.read_exact(&mut datagram).await?;
    Ok(Some(datagram))
}

/// Serve relay requests on `listener` until it fails. `allowed` is every
/// address range the listener will dial for a peer: the container overlay.
/// `keys` is read per connection, so a rotated key applies at once, and a
/// node with no key refuses everyone.
pub async fn serve(listener: TcpListener, keys: KeyWatch, allowed: Vec<IpNet>) -> io::Result<()> {
    let handshakes = Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES));
    let relayed = Arc::new(Semaphore::new(MAX_RELAYED));
    let allowed = Arc::new(allowed);
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) if crate::expose::is_transient_accept_error(&e) => {
                warn!(error = %e, "relay accept failed, retrying");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            Err(e) => return Err(e),
        };
        let Ok(handshake) = handshakes.clone().try_acquire_owned() else {
            warn!(%peer, "too many relay connections mid-handshake, dropping this one");
            continue;
        };
        let Ok(slot) = relayed.clone().try_acquire_owned() else {
            warn!(%peer, "too many relayed connections, dropping this one");
            continue;
        };
        let keys = keys.clone();
        let allowed = allowed.clone();
        tokio::spawn(async move {
            let _slot = slot;
            if let Err(e) = handle(stream, peer, &keys, &allowed, handshake).await {
                // A peer or container going away mid stream is routine.
                debug!(%peer, error = %e, "relayed connection closed");
            }
        });
    }
}

async fn handle(
    mut stream: TcpStream,
    peer: SocketAddr,
    keys: &KeyWatch,
    allowed: &[IpNet],
    handshake: tokio::sync::OwnedSemaphorePermit,
) -> io::Result<()> {
    let key = keys.borrow().clone();
    let Some(key) = key else {
        debug!(%peer, "refusing relay connection, this node holds no cluster key");
        return Ok(());
    };
    if let Err(e) = stream_auth::accept(&mut stream, &key, AUTH_LABEL).await {
        warn!(%peer, error = %e, "relay peer failed authentication, dropping the connection");
        return Ok(());
    }
    let (kind, target) = tokio::time::timeout(REQUEST_TIMEOUT, read_request(&mut stream))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no relay request in time"))??;
    drop(handshake);
    let _ = stream.set_nodelay(true);

    if !allowed.iter().any(|net| net.contains(&target.ip())) {
        warn!(%peer, %target, "refusing to relay to an address outside the container overlay");
        return write_status(
            &mut stream,
            Err(format!(
                "{target} is not a container address this node relays to"
            )),
        )
        .await;
    }

    match kind {
        Kind::Tcp => {
            let connected =
                tokio::time::timeout(TARGET_CONNECT_TIMEOUT, TcpStream::connect(target))
                    .await
                    .unwrap_or_else(|_| {
                        Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "did not accept in time",
                        ))
                    });
            let mut backend = match connected {
                Ok(backend) => backend,
                Err(e) => return write_status(&mut stream, Err(e.to_string())).await,
            };
            let _ = backend.set_nodelay(true);
            write_status(&mut stream, Ok(())).await?;
            tokio::io::copy_bidirectional(&mut stream, &mut backend).await?;
            Ok(())
        }
        Kind::Udp => {
            let local: SocketAddr = if target.is_ipv4() {
                (Ipv4Addr::UNSPECIFIED, 0).into()
            } else {
                (Ipv6Addr::UNSPECIFIED, 0).into()
            };
            let socket = match UdpSocket::bind(local).await {
                Ok(socket) => socket,
                Err(e) => return write_status(&mut stream, Err(e.to_string())).await,
            };
            if let Err(e) = socket.connect(target).await {
                return write_status(&mut stream, Err(e.to_string())).await;
            }
            write_status(&mut stream, Ok(())).await?;
            pump_udp(stream, socket).await
        }
    }
}

/// Carry one relayed UDP flow: frames from the stream go out the connected
/// socket as datagrams, datagrams back from the target go into the stream as
/// frames, until the client closes the stream or the flow goes idle.
async fn pump_udp(stream: TcpStream, socket: UdpSocket) -> io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let socket = Arc::new(socket);
    let started = Instant::now();
    let last_seen = Arc::new(AtomicU64::new(0));
    let touch = move |last_seen: &AtomicU64| {
        last_seen.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
    };

    let outbound = socket.clone();
    let outbound_seen = last_seen.clone();
    let mut inbound = tokio::spawn(async move {
        while let Some(datagram) = read_datagram(&mut reader).await? {
            touch(&outbound_seen);
            // Lossy like any UDP hop: an ICMP error from an earlier datagram
            // surfacing here must not end the flow.
            let _ = outbound.send(&datagram).await;
        }
        Ok::<(), io::Error>(())
    });

    let mut buf = vec![0u8; MAX_DATAGRAM];
    let result = loop {
        let idle_for = Duration::from_millis(
            (started.elapsed().as_millis() as u64)
                .saturating_sub(last_seen.load(Ordering::Relaxed)),
        );
        let Some(remaining) = UDP_RELAY_IDLE.checked_sub(idle_for) else {
            break Ok(());
        };
        tokio::select! {
            done = &mut inbound => break done.unwrap_or(Ok(())),
            received = tokio::time::timeout(remaining, socket.recv(&mut buf)) => match received {
                Ok(Ok(len)) => {
                    touch(&last_seen);
                    if let Err(e) = write_datagram(&mut writer, &buf[..len]).await {
                        break Err(e);
                    }
                }
                // An ICMP error for an earlier datagram, not the end of the flow.
                Ok(Err(_)) => tokio::time::sleep(Duration::from_millis(10)).await,
                Err(_) => {}
            },
        }
    };
    inbound.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_secret::ClusterKey;

    fn keys() -> KeyWatch {
        cl_raft::fixed_key(Arc::new(ClusterKey::generate()))
    }

    fn loopback() -> Vec<IpNet> {
        vec!["127.0.0.0/8".parse().unwrap()]
    }

    async fn relay(keys: KeyWatch, allowed: Vec<IpNet>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, keys, allowed));
        addr
    }

    async fn echo_backend() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut conn, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let (mut r, mut w) = conn.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        addr
    }

    async fn free_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    }

    const DIAL: Duration = Duration::from_secs(5);

    #[test]
    fn the_relay_sits_just_above_the_drive_transport() {
        assert_eq!(relay_port(7373), 7375);
    }

    #[tokio::test]
    async fn requests_round_trip_for_both_families() {
        for target in ["10.244.3.8:8080", "[fd00::8]:53"] {
            let target: SocketAddr = target.parse().unwrap();
            let encoded = encode_request(Kind::Udp, target);
            let decoded = read_request(&mut encoded.as_slice()).await.unwrap();
            assert_eq!(decoded, (Kind::Udp, target));
        }
    }

    #[tokio::test]
    async fn an_error_status_carries_its_reason() {
        let mut wire = Vec::new();
        write_status(&mut wire, Err("connection refused".into()))
            .await
            .unwrap();
        let err = read_status(&mut wire.as_slice()).await.unwrap_err();
        assert_eq!(err.to_string(), "connection refused");
    }

    #[tokio::test]
    async fn datagram_frames_round_trip_and_end_cleanly() {
        let mut wire = Vec::new();
        write_datagram(&mut wire, b"one").await.unwrap();
        write_datagram(&mut wire, b"").await.unwrap();
        let mut reader = wire.as_slice();
        assert_eq!(
            read_datagram(&mut reader).await.unwrap(),
            Some(b"one".to_vec())
        );
        assert_eq!(read_datagram(&mut reader).await.unwrap(), Some(Vec::new()));
        assert_eq!(read_datagram(&mut reader).await.unwrap(), None);
    }

    /// The whole point: a container another node runs is reached through
    /// that node, and the stream that comes back is the container's.
    #[tokio::test]
    async fn a_relayed_tcp_connection_reaches_the_target() {
        let keys = keys();
        let relay = relay(keys.clone(), loopback()).await;
        let backend = echo_backend().await;

        let dialer = Dialer::new(keys);
        dialer.set_relays(HashMap::from([(backend.ip(), relay)]));
        let mut stream = dialer.connect_tcp(backend, DIAL).await.unwrap();
        stream.write_all(b"through the owner").await.unwrap();
        let mut buf = [0u8; 17];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"through the owner");
    }

    /// An address with no relay entry is this node's own container and must
    /// keep the direct path; an address with one must not bypass the relay.
    #[tokio::test]
    async fn only_addresses_in_the_table_go_through_a_relay() {
        let backend = echo_backend().await;
        let dialer = Dialer::new(keys());
        dialer
            .connect_tcp(backend, DIAL)
            .await
            .expect("no relay entry: dialed directly");

        let nobody = free_addr().await;
        dialer.set_relays(HashMap::from([(backend.ip(), nobody)]));
        let err = dialer.connect_tcp(backend, DIAL).await.unwrap_err();
        assert!(
            err.to_string().contains(&format!("relay at {nobody}")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_target_outside_the_overlay_is_refused() {
        let keys = keys();
        let relay = relay(keys.clone(), vec!["10.244.0.0/16".parse().unwrap()]).await;
        let backend = echo_backend().await;

        let dialer = Dialer::new(keys);
        dialer.set_relays(HashMap::from([(backend.ip(), relay)]));
        let err = dialer.connect_tcp(backend, DIAL).await.unwrap_err();
        assert!(err.to_string().contains("not a container address"), "{err}");
    }

    #[tokio::test]
    async fn a_target_that_refuses_is_reported_by_the_relay() {
        let keys = keys();
        let relay = relay(keys.clone(), loopback()).await;
        let dead = free_addr().await;

        let dialer = Dialer::new(keys);
        dialer.set_relays(HashMap::from([(dead.ip(), relay)]));
        let err = dialer.connect_tcp(dead, DIAL).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused, "{err}");
        assert!(
            err.to_string().contains(&format!("could not reach {dead}")),
            "{err}"
        );
    }

    /// A relay is a way into every container on the node, so a peer without
    /// the cluster key gets nothing from it.
    #[tokio::test]
    async fn a_dialer_with_another_key_is_turned_away() {
        let relay = relay(keys(), loopback()).await;
        let backend = echo_backend().await;

        let dialer = Dialer::new(keys());
        dialer.set_relays(HashMap::from([(backend.ip(), relay)]));
        assert!(dialer.connect_tcp(backend, DIAL).await.is_err());
    }

    #[tokio::test]
    async fn a_dialer_without_a_key_says_so() {
        let backend = echo_backend().await;
        let dialer = Dialer::direct();
        dialer.set_relays(HashMap::from([(backend.ip(), free_addr().await)]));
        let err = dialer.connect_tcp(backend, DIAL).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    }

    #[tokio::test]
    async fn a_relayed_udp_flow_carries_datagrams_both_ways() {
        let keys = keys();
        let relay = relay(keys.clone(), loopback()).await;
        let backend = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = backend.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            while let Ok((len, from)) = backend.recv_from(&mut buf).await {
                let _ = backend.send_to(&buf[..len], from).await;
            }
        });

        let dialer = Dialer::new(keys);
        let mut stream = dialer.connect_udp(relay, target, DIAL).await.unwrap();
        for datagram in [&b"first"[..], &b"second"[..]] {
            write_datagram(&mut stream, datagram).await.unwrap();
            let echoed = tokio::time::timeout(DIAL, read_datagram(&mut stream))
                .await
                .expect("an answer")
                .unwrap();
            assert_eq!(echoed.as_deref(), Some(datagram));
        }
    }
}
