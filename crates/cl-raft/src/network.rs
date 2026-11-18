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

/// Label both ends authenticate under, so a captured handshake cannot open any other channel.
pub const AUTH_LABEL: &str = "raft";

/// Live view of this process's cluster key, which may arrive after boot; read per connection.
pub type KeyWatch = watch::Receiver<Option<Arc<ClusterKey>>>;

/// A `KeyWatch` that always holds `key` (e.g. the cli).
pub fn fixed_key(key: Arc<ClusterKey>) -> KeyWatch {
    // The sender is dropped; a receiver keeps serving its last value.
    watch::channel(Some(key)).1
}

/// A `KeyWatch` with a fresh random key, for tests; clone the receiver into every node.
pub fn throwaway_key() -> KeyWatch {
    fixed_key(Arc::new(ClusterKey::generate()))
}

/// How long a detached raft exchange may run before it is abandoned. Far above
/// raft's own deadlines, which only bound how long raft waits, not the work.
const DETACHED_CALL_LIMIT: Duration = Duration::from_secs(10);

type NetError<E = RaftError<NodeId>> = RPCError<NodeId, BasicNode, E>;

/// Hands raft a client per peer; connections are cached and reopened lazily.
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
        // Keyed by the address as well as the id: a node re-addressed through
        // a membership change keeps its id, and a cached connection to its old
        // address would otherwise be handed out forever.
        match peers.get(&id) {
            Some(peer) if peer.addr == node.addr => peer.clone(),
            _ => {
                let peer = Arc::new(Peer::new(node.addr.clone(), self.keys.clone()));
                peers.insert(id, peer.clone());
                peer
            }
        }
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

/// One peer's cached connection. The mutex serializes requests: replies are not tagged.
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

    pub fn connect(addr: &str, keys: KeyWatch) -> Self {
        Self::new(addr.to_string(), keys)
    }

    /// `call` with an upper bound, so a dead peer cannot hold the connection lock.
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

    pub async fn call(&self, request: &RpcRequest) -> Result<RpcResponse, std::io::Error> {
        let mut guard = self.conn.lock().await;

        // The socket is only put back after a completed exchange: if this future is
        // cancelled mid-exchange it is dropped, since a cached socket with an unread
        // reply would shift every later answer by one (replies are untagged).
        if let Some(mut conn) = guard.take() {
            // Wrong-kind reply: socket is out of step, drop it.
            if let Ok(response) = Self::exchange(&mut conn, request).await {
                if reply_fits(request, &response) {
                    *guard = Some(conn);
                    return Ok(response);
                }
            }
            // Peer restarted and left a dead socket: retry once on a fresh one.
        }

        // No key: the listener would refuse us; fail here with a clear reason.
        let key = self.keys.borrow().clone().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "this node holds no cluster key, so it cannot authenticate to its peers",
            )
        })?;
        let mut stream = TcpStream::connect(&self.addr).await?;
        // Mutual proof of the cluster key before any frame, so we never ship writes or snapshots to a non-member.
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

pub struct Channel {
    target: NodeId,
    peer: Arc<Peer>,
}

impl Channel {
    /// Send `request`, giving up after `deadline`. A connect to a dead peer can
    /// outlast raft's heartbeat timeout, which surfaces as `RPCError::Timeout` and
    /// is never backed off (log flood); timing out here yields `Unreachable`,
    /// which raft does back off on.
    async fn call<E>(
        &self,
        request: RpcRequest,
        deadline: Duration,
    ) -> Result<RpcResponse, NetError<E>>
    where
        E: std::error::Error,
    {
        // Runs detached so the deadline only stops us waiting: cancelling mid-connect
        // would restart the handshake next time, and one lost SYN would make every
        // attempt miss its deadline. Left to finish, the connection is cached for the
        // next heartbeat, and the task consumes its own response so a late answer
        // is never read by an unrelated call.
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

    /// Grow the backoff so a long-downed node does not log a warn+error pair per heartbeat at raft's flat 500ms default.
    fn backoff(&self) -> Backoff {
        let schedule = [1, 1, 2, 5].into_iter().map(Duration::from_secs);
        Backoff::new(schedule.chain(std::iter::repeat(Duration::from_secs(5))))
    }
}

/// Whether `response` is a possible answer to `request` (replies are untagged, so this detects an out-of-step socket). `Error` answers anything.
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

pub async fn ping(peer: &Peer) -> bool {
    matches!(peer.call(&RpcRequest::Ping).await, Ok(RpcResponse::Pong))
}

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
