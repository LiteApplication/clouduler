//! The raft network implementation on top of the node RPC channel.

#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::sync::Arc;

use std::time::Duration;

use cl_secret::stream_auth;
use cl_secret::ClusterKey;
use futures::{SinkExt, StreamExt};
use openraft::error::{NetworkError, RPCError, RaftError, Unreachable};
use openraft::network::{Backoff, RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::{AnyError, BasicNode};
use tokio::net::TcpStream;
use tokio::sync::{watch, Mutex};

use crate::ids::NodeId;
use crate::rpc::{decode, encode, frame, Connection, RpcRequest, RpcResponse};
use crate::state::{Request as StateRequest, Response as StateResponse};
use crate::types::TypeConfig;

/// The label both ends of the raft channel authenticate under, so a handshake
/// captured here cannot be used to open any other node-to-node channel.
pub const AUTH_LABEL: &str = "raft";

/// A live view of this process's cluster key. The key can arrive after the node
/// starts (another node delivers it), so connections read it when they are made
/// rather than capturing it at boot.
pub type KeyWatch = watch::Receiver<Option<Arc<ClusterKey>>>;

/// A `KeyWatch` that always holds `key`, for callers with a key in hand and no
/// need for it to change, like the cli.
pub fn fixed_key(key: Arc<ClusterKey>) -> KeyWatch {
    // The sender is dropped straight away; a receiver keeps serving the last
    // value it saw, which is all a fixed key needs.
    watch::channel(Some(key)).1
}

/// A `KeyWatch` holding a fresh random key, for tests that stand up nodes and
/// have no key file to load. Nodes only talk to each other when they share one,
/// so a test cluster clones the receiver it gets from here into every node.
pub fn throwaway_key() -> KeyWatch {
    fixed_key(Arc::new(ClusterKey::generate()))
}

/// How long a detached raft exchange may run before it is abandoned. Far above
/// raft's own deadlines, which only bound how long raft waits, not the work.
const DETACHED_CALL_LIMIT: Duration = Duration::from_secs(10);

type NetError<E = RaftError<NodeId>> = RPCError<NodeId, BasicNode, E>;

/// Hands raft a client per peer. Connections are cached and reopened lazily,
/// since heartbeats would otherwise pay a TCP handshake every tick.
#[derive(Clone)]
pub struct Network {
    peers: Arc<Mutex<BTreeMap<NodeId, Arc<Peer>>>>,
    keys: KeyWatch,
}

impl Network {
    pub fn new(keys: KeyWatch) -> Self {
        Self {
            peers: Arc::default(),
            keys,
        }
    }

    async fn peer(&self, id: NodeId, node: &BasicNode) -> Arc<Peer> {
        let mut peers = self.peers.lock().await;
        peers
            .entry(id)
            .or_insert_with(|| Arc::new(Peer::new(node.addr.clone(), self.keys.clone())))
            .clone()
    }
}

impl RaftNetworkFactory<TypeConfig> for Network {
    type Network = Channel;

    async fn new_client(&mut self, target: NodeId, node: &BasicNode) -> Self::Network {
        Channel {
            target,
            peer: self.peer(target, node).await,
        }
    }
}

/// One peer's cached connection. The mutex serializes requests, which is what
/// the single socket needs anyway since responses are not tagged.
pub struct Peer {
    addr: String,
    keys: KeyWatch,
    conn: Mutex<Option<Connection>>,
}

impl Peer {
    fn new(addr: String, keys: KeyWatch) -> Self {
        Self {
            addr,
            keys,
            conn: Mutex::new(None),
        }
    }

    /// A one off peer, for callers outside raft's own replication paths.
    pub fn connect(addr: &str, keys: KeyWatch) -> Self {
        Self::new(addr.to_string(), keys)
    }

    /// `call` with an upper bound, for callers that run it detached and so
    /// cannot rely on a caller's deadline to stop a dead peer holding the
    /// connection lock.
    async fn call_bounded(&self, request: &RpcRequest) -> Result<RpcResponse, std::io::Error> {
        tokio::time::timeout(DETACHED_CALL_LIMIT, self.call(request))
            .await
            .unwrap_or_else(|_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "rpc exchange timed out",
                ))
            })
    }

    /// Send one request, reconnecting once if the cached socket is stale.
    pub async fn call(&self, request: &RpcRequest) -> Result<RpcResponse, std::io::Error> {
        let mut guard = self.conn.lock().await;

        // Take the socket out while it is in use and only put it back once the
        // exchange completed. If this future is cancelled mid exchange (the
        // detached call limit, or a caller's timeout) the socket is dropped
        // with it; leaving it cached would keep an unread reply on it, and
        // since replies are not tagged every later call would read the answer
        // to the one before.
        if let Some(mut conn) = guard.take() {
            // A reply of the wrong kind means the socket is out of step with
            // us (a stale answer is queued ahead of the real one), so it is
            // dropped like a dead one rather than trusted for another call.
            if let Ok(response) = Self::exchange(&mut conn, request).await {
                if reply_fits(request, &response) {
                    *guard = Some(conn);
                    return Ok(response);
                }
            }
            // Otherwise a peer that restarted left us a dead socket. Drop it
            // and try once more on a fresh one.
        }

        // No key means we cannot prove membership, and the listener would
        // refuse us anyway; failing here says why instead of a bare reset.
        let key = self.keys.borrow().clone().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "this node holds no cluster key, so it cannot authenticate to its peers",
            )
        })?;
        let mut stream = TcpStream::connect(&self.addr).await?;
        // Mutual proof of the cluster key before any frame is exchanged, so
        // we never ship a write or a snapshot to something that is not a
        // member.
        stream_auth::connect(&mut stream, &key, AUTH_LABEL)
            .await
            .map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("authenticating to {}: {e}", self.addr),
                )
            })?;
        let mut conn = frame(stream);
        let response = Self::exchange(&mut conn, request).await?;
        *guard = Some(conn);
        Ok(response)
    }

    async fn exchange(
        conn: &mut Connection,
        request: &RpcRequest,
    ) -> Result<RpcResponse, std::io::Error> {
        conn.send(encode(request)?).await?;
        match conn.next().await {
            Some(Ok(raw)) => decode(&raw),
            Some(Err(e)) => Err(e),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "peer closed the connection",
            )),
        }
    }
}

/// A raft client bound to one peer.
pub struct Channel {
    target: NodeId,
    peer: Arc<Peer>,
}

impl Channel {
    /// Send `request`, giving up after `deadline` rather than letting a dead
    /// peer's TCP connect hang until raft's own hard_ttl fires. A connect
    /// that never gets a RST (peer down, firewalled) can take far longer
    /// than the ~500ms heartbeat timeout, and if we let raft's own timer
    /// expire instead of ours we surface as `RPCError::Timeout`, which raft
    /// never backs off on — that's what floods the log every heartbeat for
    /// as long as the peer stays down. Timing out here first means we report
    /// `Unreachable`, which raft *does* back off on.
    async fn call<E>(
        &self,
        request: RpcRequest,
        deadline: Duration,
    ) -> Result<RpcResponse, NetError<E>>
    where
        E: std::error::Error,
    {
        // The exchange runs detached so a deadline only stops us waiting for
        // it. Cancelling it mid connect would restart the TCP and auth
        // handshake from scratch on the next attempt, so one lost SYN (a
        // one second retransmit) would make every attempt miss its deadline
        // and keep the peer in raft's growing backoff. Left to finish, the
        // slow connection is cached and the next heartbeat finds it ready,
        // and since the task consumes its own response a late answer can
        // never be read by an unrelated call.
        let peer = self.peer.clone();
        let task = tokio::spawn(async move { peer.call_bounded(&request).await });
        match tokio::time::timeout(deadline, task).await {
            Ok(Ok(Ok(response))) => Ok(response),
            Ok(Ok(Err(e))) => Err(RPCError::Unreachable(Unreachable::new(&e))),
            Ok(Err(join)) => Err(RPCError::Unreachable(Unreachable::new(&join))),
            Err(elapsed) => Err(RPCError::Unreachable(Unreachable::new(&elapsed))),
        }
    }

    fn unexpected<E>(&self, response: RpcResponse) -> NetError<E>
    where
        E: std::error::Error,
    {
        let msg = match response {
            RpcResponse::Error(msg) => msg,
            other => format!("unexpected reply {other:?}"),
        };
        RPCError::Network(NetworkError::new(&AnyError::error(format!(
            "node {}: {msg}",
            self.target
        ))))
    }
}

impl RaftNetwork<TypeConfig> for Channel {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, NetError> {
        match self
            .call(RpcRequest::AppendEntries(rpc), option.soft_ttl())
            .await?
        {
            RpcResponse::AppendEntries(response) => Ok(response),
            other => Err(self.unexpected(other)),
        }
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        NetError<RaftError<NodeId, openraft::error::InstallSnapshotError>>,
    > {
        match self
            .call(RpcRequest::InstallSnapshot(rpc), option.soft_ttl())
            .await?
        {
            RpcResponse::InstallSnapshot(response) => Ok(response),
            other => Err(self.unexpected(other)),
        }
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, NetError> {
        match self.call(RpcRequest::Vote(rpc), option.soft_ttl()).await? {
            RpcResponse::Vote(response) => Ok(response),
            other => Err(self.unexpected(other)),
        }
    }

    /// A dead peer otherwise gets retried at raft's default flat 500ms
    /// backoff, which is indistinguishable in the logs from no backoff at
    /// all. Grow it so a long-downed node quiets down instead of logging a
    /// warn+error pair on every heartbeat forever.
    fn backoff(&self) -> Backoff {
        let schedule = [1, 1, 2, 5].into_iter().map(Duration::from_secs);
        Backoff::new(schedule.chain(std::iter::repeat(Duration::from_secs(5))))
    }
}

/// Whether `response` is a possible answer to `request`. Replies are not
/// tagged, so this is the only way to notice a socket that has fallen out of
/// step. `Error` answers anything.
fn reply_fits(request: &RpcRequest, response: &RpcResponse) -> bool {
    matches!(
        (request, response),
        (_, RpcResponse::Error(_))
            | (RpcRequest::AppendEntries(_), RpcResponse::AppendEntries(_))
            | (
                RpcRequest::InstallSnapshot(_),
                RpcResponse::InstallSnapshot(_)
            )
            | (RpcRequest::Vote(_), RpcResponse::Vote(_))
            | (RpcRequest::Forward(_), RpcResponse::Forward(_))
            | (RpcRequest::Query(_), RpcResponse::Query(_))
            | (RpcRequest::Ping, RpcResponse::Pong)
    )
}

/// Probe a peer's liveness. Used by the leader's failure detector.
pub async fn ping(peer: &Peer) -> bool {
    matches!(peer.call(&RpcRequest::Ping).await, Ok(RpcResponse::Pong))
}

/// Forward a write to the leader over the same channel raft uses.
pub async fn forward(peer: &Peer, request: StateRequest) -> Result<StateResponse, String> {
    match peer.call(&RpcRequest::Forward(request)).await {
        Ok(RpcResponse::Forward(result)) => result,
        Ok(RpcResponse::Error(msg)) => Err(msg),
        Ok(other) => Err(format!("unexpected reply {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn a_cancelled_exchange_does_not_leave_a_poisoned_socket_cached() {
        // A peer that accepts and reads but never answers.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 1024];
            while s.read(&mut buf).await.map(|n| n > 0).unwrap_or(false) {}
        });

        let peer = Peer::connect(&addr.to_string(), throwaway_key());
        let stream = TcpStream::connect(addr).await.expect("connect");
        *peer.conn.lock().await = Some(frame(stream));

        let outcome =
            tokio::time::timeout(Duration::from_millis(100), peer.call(&RpcRequest::Ping)).await;
        assert!(outcome.is_err(), "the silent peer should time the call out");
        assert!(
            peer.conn.lock().await.is_none(),
            "a cancelled exchange must not leave its socket cached"
        );
    }

    #[test]
    fn only_the_matching_reply_kind_fits() {
        assert!(reply_fits(&RpcRequest::Ping, &RpcResponse::Pong));
        assert!(reply_fits(
            &RpcRequest::Ping,
            &RpcResponse::Error("x".into())
        ));
        assert!(!reply_fits(
            &RpcRequest::Ping,
            &RpcResponse::Forward(Err("stale".into()))
        ));
    }
}
