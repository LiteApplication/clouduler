//! Mutual authentication of a raw byte stream with the cluster key.
//!
//! Node-to-node channels (raft rpc, drive sync) are plain TCP on the internal
//! network. "Internal" is not a trust boundary: any LAN host, or a container
//! that can route to a node's internal address, can open the socket. So before
//! either side wraps the stream in its frame codec, both prove they hold the
//! cluster key, the same trust boundary `internal_auth` uses for the http side.
//!
//! The exchange is a challenge-response in both directions, so a listener is
//! not fooled by an impostor client and a client is not fooled by an impostor
//! listener (which matters because a client may be about to ship a snapshot or
//! a secret-bearing write to whatever answers at that address):
//!
//! ```text
//! server -> client   MAGIC (4 bytes) || server_nonce (32)
//! client -> server   client_nonce (32) || HMAC(key, "{label}:client:" || sn || cn)
//! server -> client   HMAC(key, "{label}:server:" || sn || cn)
//! ```
//!
//! Both nonces are fresh per connection and both MACs cover both nonces, so a
//! recorded handshake cannot be replayed on another connection. The role tag
//! in each MAC stops one side's proof being reflected back as the other's. The
//! `label` ("raft", "drive") scopes a proof to one protocol, so a captured
//! handshake for one channel cannot open another. The server speaks first so
//! an unauthenticated peer is only ever handed a random nonce.
//!
//! This authenticates the connection, it does not encrypt or MAC the traffic
//! that follows. That is the same level of protection the channel had against
//! passive observers before, and the goal here is to stop unauthenticated
//! parties from *speaking* to the node, not to hide the contents on the wire.

use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::key::ClusterKey;

/// Sent by the listener ahead of its nonce. Doubles as a protocol version: a
/// node on a different (or no) handshake version fails with a clear error
/// instead of misreading bytes as a nonce. Bump the last byte on any change.
const MAGIC: [u8; 4] = *b"CLA1";

const NONCE_BYTES: usize = 32;
const MAC_BYTES: usize = 32;

/// How long a whole handshake may take. Bounds how long an unauthenticated
/// connection can hold a task and a socket on the listener, and how long a
/// client waits on a peer that accepts the connection but never answers.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("handshake timed out")]
    Timeout,

    #[error("handshake i/o: {0}")]
    Io(#[from] std::io::Error),

    /// The listener did not open with our magic: an old node, or not a
    /// clouduler listener at all.
    #[error("peer is not speaking this handshake version")]
    BadMagic,

    /// The client's proof did not verify, so it does not hold the cluster key.
    #[error("peer failed to prove it holds the cluster key")]
    ClientRejected,

    /// The listener's proof did not verify, so whatever answered is not a
    /// member of this cluster.
    #[error("peer is not authentic: it failed to prove it holds the cluster key")]
    ServerRejected,
}

/// Run the listener side on a freshly accepted stream, before any framing.
/// On `Ok` the peer holds the cluster key; on `Err` the caller must drop the
/// stream.
pub async fn accept<S>(stream: &mut S, key: &ClusterKey, label: &str) -> Result<(), AuthError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    accept_within(stream, key, label, HANDSHAKE_TIMEOUT).await
}

/// Run the dialing side on a freshly connected stream, before any framing.
pub async fn connect<S>(stream: &mut S, key: &ClusterKey, label: &str) -> Result<(), AuthError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    connect_within(stream, key, label, HANDSHAKE_TIMEOUT).await
}

/// `accept` with an explicit deadline, for callers (and tests) that need a
/// different bound than `HANDSHAKE_TIMEOUT`.
pub async fn accept_within<S>(
    stream: &mut S,
    key: &ClusterKey,
    label: &str,
    deadline: Duration,
) -> Result<(), AuthError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(deadline, accept_inner(stream, key, label))
        .await
        .map_err(|_| AuthError::Timeout)?
}

/// `connect` with an explicit deadline.
pub async fn connect_within<S>(
    stream: &mut S,
    key: &ClusterKey,
    label: &str,
    deadline: Duration,
) -> Result<(), AuthError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(deadline, connect_inner(stream, key, label))
        .await
        .map_err(|_| AuthError::Timeout)?
}

async fn accept_inner<S>(stream: &mut S, key: &ClusterKey, label: &str) -> Result<(), AuthError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut server_nonce = [0u8; NONCE_BYTES];
    rand::fill(&mut server_nonce);

    let mut hello = [0u8; MAGIC.len() + NONCE_BYTES];
    hello[..MAGIC.len()].copy_from_slice(&MAGIC);
    hello[MAGIC.len()..].copy_from_slice(&server_nonce);
    stream.write_all(&hello).await?;
    stream.flush().await?;

    let mut reply = [0u8; NONCE_BYTES + MAC_BYTES];
    stream.read_exact(&mut reply).await?;
    let (client_nonce, presented) = reply.split_at(NONCE_BYTES);

    // `verify_slice` compares in constant time, so how much of a forged proof
    // was right does not leak through timing.
    let expected = mac(key, label, "client", &server_nonce, client_nonce);
    if expected.verify_slice(presented).is_err() {
        return Err(AuthError::ClientRejected);
    }

    let proof = mac(key, label, "server", &server_nonce, client_nonce)
        .finalize()
        .into_bytes();
    stream.write_all(&proof).await?;
    stream.flush().await?;
    Ok(())
}

async fn connect_inner<S>(stream: &mut S, key: &ClusterKey, label: &str) -> Result<(), AuthError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut hello = [0u8; MAGIC.len() + NONCE_BYTES];
    stream.read_exact(&mut hello).await?;
    if hello[..MAGIC.len()] != MAGIC {
        return Err(AuthError::BadMagic);
    }
    let server_nonce = &hello[MAGIC.len()..];

    let mut client_nonce = [0u8; NONCE_BYTES];
    rand::fill(&mut client_nonce);

    let proof = mac(key, label, "client", server_nonce, &client_nonce)
        .finalize()
        .into_bytes();
    let mut reply = [0u8; NONCE_BYTES + MAC_BYTES];
    reply[..NONCE_BYTES].copy_from_slice(&client_nonce);
    reply[NONCE_BYTES..].copy_from_slice(&proof);
    stream.write_all(&reply).await?;
    stream.flush().await?;

    let mut presented = [0u8; MAC_BYTES];
    stream.read_exact(&mut presented).await?;
    if mac(key, label, "server", server_nonce, &client_nonce)
        .verify_slice(&presented)
        .is_err()
    {
        return Err(AuthError::ServerRejected);
    }
    Ok(())
}

fn mac(
    key: &ClusterKey,
    label: &str,
    role: &str,
    server_nonce: &[u8],
    client_nonce: &[u8],
) -> Hmac<Sha256> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.bytes())
        .expect("any length key is valid for hmac-sha256");
    mac.update(label.as_bytes());
    mac.update(b":");
    mac.update(role.as_bytes());
    mac.update(b":");
    mac.update(server_nonce);
    mac.update(client_nonce);
    mac
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    const SHORT: Duration = Duration::from_millis(200);

    #[tokio::test]
    async fn both_sides_succeed_with_the_same_key() {
        let key = ClusterKey::generate();
        let (mut a, mut b) = duplex(1024);
        let (server, client) =
            tokio::join!(accept(&mut a, &key, "raft"), connect(&mut b, &key, "raft"));
        server.expect("server accepts");
        client.expect("client accepts");
    }

    #[tokio::test]
    async fn the_stream_is_usable_after_the_handshake() {
        let key = ClusterKey::generate();
        let (mut a, mut b) = duplex(1024);
        let (server, client) =
            tokio::join!(accept(&mut a, &key, "raft"), connect(&mut b, &key, "raft"));
        server.unwrap();
        client.unwrap();
        a.write_all(b"hi").await.unwrap();
        let mut got = [0u8; 2];
        b.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"hi");
    }

    #[tokio::test]
    async fn a_client_with_the_wrong_key_is_rejected_by_the_server() {
        let (mut a, mut b) = duplex(1024);
        let (server_key, client_key) = (ClusterKey::generate(), ClusterKey::generate());
        let (server, client) = tokio::join!(
            accept_within(&mut a, &server_key, "raft", SHORT),
            connect_within(&mut b, &client_key, "raft", SHORT)
        );
        assert!(matches!(server, Err(AuthError::ClientRejected)));
        // The server never sends its proof, so the client sees a closed or
        // silent stream rather than a verdict; either way it must not succeed.
        assert!(client.is_err());
    }

    #[tokio::test]
    async fn a_server_with_the_wrong_key_is_rejected_by_the_client() {
        // A server that somehow accepted the client's proof but signs its own
        // with another key: emulate by running the real client against a
        // hand-rolled listener that verifies with key A and answers with key B.
        let (a_key, b_key) = (ClusterKey::generate(), ClusterKey::generate());
        let (mut s, mut c) = duplex(1024);
        let fake_server = async {
            let sn = [7u8; NONCE_BYTES];
            let mut hello = MAGIC.to_vec();
            hello.extend_from_slice(&sn);
            s.write_all(&hello).await.unwrap();
            let mut reply = [0u8; NONCE_BYTES + MAC_BYTES];
            s.read_exact(&mut reply).await.unwrap();
            let bad = mac(&b_key, "raft", "server", &sn, &reply[..NONCE_BYTES])
                .finalize()
                .into_bytes();
            s.write_all(&bad).await.unwrap();
        };
        let (_, client) = tokio::join!(fake_server, connect(&mut c, &a_key, "raft"));
        assert!(matches!(client, Err(AuthError::ServerRejected)));
    }

    #[tokio::test]
    async fn a_different_label_does_not_authenticate() {
        let key = ClusterKey::generate();
        let (mut a, mut b) = duplex(1024);
        let (server, _client) = tokio::join!(
            accept_within(&mut a, &key, "raft", SHORT),
            connect_within(&mut b, &key, "drive", SHORT)
        );
        assert!(matches!(server, Err(AuthError::ClientRejected)));
    }

    #[tokio::test]
    async fn garbage_from_a_client_is_rejected_not_panicked_on() {
        let key = ClusterKey::generate();
        let (mut a, mut b) = duplex(1024);
        let attacker = async {
            let mut hello = [0u8; MAGIC.len() + NONCE_BYTES];
            b.read_exact(&mut hello).await.unwrap();
            b.write_all(&[0xAB; NONCE_BYTES + MAC_BYTES]).await.unwrap();
        };
        let (server, _) = tokio::join!(accept(&mut a, &key, "raft"), attacker);
        assert!(matches!(server, Err(AuthError::ClientRejected)));
    }

    #[tokio::test]
    async fn a_truncated_client_reply_is_an_error() {
        let key = ClusterKey::generate();
        let (mut a, mut b) = duplex(1024);
        let attacker = async {
            let mut hello = [0u8; MAGIC.len() + NONCE_BYTES];
            b.read_exact(&mut hello).await.unwrap();
            b.write_all(&[1, 2, 3]).await.unwrap();
            drop(b);
        };
        let (server, _) = tokio::join!(accept(&mut a, &key, "raft"), attacker);
        assert!(matches!(server, Err(AuthError::Io(_))));
    }

    #[tokio::test]
    async fn a_listener_without_our_magic_is_a_clear_error() {
        let key = ClusterKey::generate();
        let (mut s, mut c) = duplex(1024);
        s.write_all(&[0u8; MAGIC.len() + NONCE_BYTES])
            .await
            .unwrap();
        let client = connect(&mut c, &key, "raft").await;
        assert!(matches!(client, Err(AuthError::BadMagic)));
    }

    #[tokio::test]
    async fn a_silent_peer_times_out_on_both_sides() {
        let key = ClusterKey::generate();
        let (mut a, _keep_a) = duplex(1024);
        let server = accept_within(&mut a, &key, "raft", SHORT).await;
        assert!(matches!(server, Err(AuthError::Timeout)));

        let (mut b, _keep_b) = duplex(1024);
        let client = connect_within(&mut b, &key, "raft", SHORT).await;
        assert!(matches!(client, Err(AuthError::Timeout)));
    }
}
