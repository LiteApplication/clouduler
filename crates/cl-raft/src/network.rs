//! The raft network implementation on top of the node RPC channel.

#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use openraft::error::{NetworkError, RPCError, RaftError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
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
    async fn call<E>(&self, request: RpcRequest) -> Result<RpcResponse, NetError<E>>
    where
        E: std::error::Error,
    {
        self.peer.call(&request).await.map_err(|e| {
            // Unreachable rather than NetworkError, so raft backs off on this
            // peer instead of treating it as a protocol fault.
            RPCError::Unreachable(Unreachable::new(&e))
        })
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
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, NetError> {
        match self.call(RpcRequest::AppendEntries(rpc)).await? {
            RpcResponse::AppendEntries(response) => Ok(response),
            other => Err(self.unexpected(other)),
        }
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        NetError<RaftError<NodeId, openraft::error::InstallSnapshotError>>,
    > {
        match self.call(RpcRequest::InstallSnapshot(rpc)).await? {
            RpcResponse::InstallSnapshot(response) => Ok(response),
            other => Err(self.unexpected(other)),
        }
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, NetError> {
        match self.call(RpcRequest::Vote(rpc)).await? {
            RpcResponse::Vote(response) => Ok(response),
            other => Err(self.unexpected(other)),
        }
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
