//! The raft network implementation on top of the node RPC channel.

#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::sync::Arc;

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use openraft::error::{NetworkError, RPCError, RaftError, Unreachable};
use openraft::network::{Backoff, RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::{AnyError, BasicNode};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::ids::NodeId;
use crate::rpc::{decode, encode, frame, Connection, RpcRequest, RpcResponse};
use crate::state::{Request as StateRequest, Response as StateResponse};
use crate::types::TypeConfig;

type NetError<E = RaftError<NodeId>> = RPCError<NodeId, BasicNode, E>;

/// Hands raft a client per peer. Connections are cached and reopened lazily,
/// since heartbeats would otherwise pay a TCP handshake every tick.
#[derive(Clone, Default)]
pub struct Network {
    peers: Arc<Mutex<BTreeMap<NodeId, Arc<Peer>>>>,
}

impl Network {
    pub fn new() -> Self {
        Self::default()
    }

    async fn peer(&self, id: NodeId, node: &BasicNode) -> Arc<Peer> {
        let mut peers = self.peers.lock().await;
        peers
            .entry(id)
            .or_insert_with(|| Arc::new(Peer::new(node.addr.clone())))
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
    conn: Mutex<Option<Connection>>,
}

impl Peer {
    fn new(addr: String) -> Self {
        Self {
            addr,
            conn: Mutex::new(None),
        }
    }

    /// A one off peer, for callers outside raft's own replication paths.
    pub fn connect(addr: &str) -> Self {
        Self::new(addr.to_string())
    }

    /// Drop the cached connection, if any, without waiting on whatever
    /// in-flight exchange might still be holding it.
    ///
    /// Must be called whenever a caller gives up on a request via its own
    /// deadline (see `Channel::call`) rather than a real socket error: `call`
    /// pairs requests and responses purely by send-then-read order on one
    /// shared socket, so if a caller stops waiting mid-exchange the request
    /// it sent is still in flight and its response is still coming — read by
    /// this same connection later, it would be handed to a completely
    /// unrelated call as its answer. Forcing a reconnect here throws that
    /// stale response away with the old socket instead of letting it
    /// desynchronize every future exchange on this peer.
    async fn invalidate(&self) {
        *self.conn.lock().await = None;
    }

    /// Send one request, reconnecting once if the cached socket is stale.
    pub async fn call(&self, request: &RpcRequest) -> Result<RpcResponse, std::io::Error> {
        let mut guard = self.conn.lock().await;

        if guard.is_some() {
            match Self::exchange(guard.as_mut().expect("checked"), request).await {
                Ok(response) => return Ok(response),
                // A peer that restarted leaves us holding a dead socket. Drop it
                // and try once more before giving up.
                Err(_) => *guard = None,
            }
        }

        let stream = TcpStream::connect(&self.addr).await?;
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
    async fn call<E>(&self, request: RpcRequest, deadline: Duration) -> Result<RpcResponse, NetError<E>>
    where
        E: std::error::Error,
    {
        match tokio::time::timeout(deadline, self.peer.call(&request)).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(e)) => Err(RPCError::Unreachable(Unreachable::new(&e))),
            Err(elapsed) => {
                // The request we just sent is still in flight on that socket
                // with no one left reading its response; see `invalidate`'s
                // doc comment for why the cached connection can't be reused
                // as-is after this.
                self.peer.invalidate().await;
                Err(RPCError::Unreachable(Unreachable::new(&elapsed)))
            }
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
        match self.call(RpcRequest::AppendEntries(rpc), option.soft_ttl()).await? {
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
        match self.call(RpcRequest::InstallSnapshot(rpc), option.soft_ttl()).await? {
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
        let schedule = [1, 2, 5, 10, 30].into_iter().map(Duration::from_secs);
        Backoff::new(schedule.chain(std::iter::repeat(Duration::from_secs(30))))
    }
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
