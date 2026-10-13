//! Node to node RPC.
//!
//! Length prefixed json over plain TCP on the internal network. No TLS: the
//! internal network is trusted by definition, and skipping it keeps the same
//! channel cheap enough to also carry drive sync and proxy backhaul later.

use std::io;

use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::ids::NodeId;
use crate::state::{Request as StateRequest, Response as StateResponse};
use crate::types::TypeConfig;

/// Refuse anything larger than this so a corrupt length prefix cannot make us
/// allocate wildly. Snapshots are chunked well below it.
const MAX_FRAME: usize = 64 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub enum RpcRequest {
    AppendEntries(AppendEntriesRequest<TypeConfig>),
    InstallSnapshot(InstallSnapshotRequest<TypeConfig>),
    Vote(VoteRequest<NodeId>),
    /// A write submitted on a follower, forwarded to the leader.
    Forward(StateRequest),
    /// A read served from the local applied state.
    Query(Query),
    /// Liveness probe used by the quorum watcher.
    Ping,
}

/// Reads are answered from whichever node is asked, out of its own applied
/// state. That can lag the leader by the replication delay, which is fine for
/// the cli and the ui; anything needing linearizability goes through a write.
#[derive(Debug, Serialize, Deserialize)]
pub enum Query {
    /// Quorum, leader and version, without shipping the whole state.
    Status,
    /// The full applied state.
    State {
        /// Secrets and private keys are blanked unless this is set. Only the
        /// cli's backup snapshot/restore paths set it, to open a
        /// destination's sealed password locally.
        include_secrets: bool,
    },
}

/// What a node reports about itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    pub node: String,
    pub id: NodeId,
    pub leader: Option<NodeId>,
    pub is_leader: bool,
    pub has_quorum: bool,
    pub voters: usize,
    pub reachable: usize,
    /// State machine version, so a caller can tell how far behind this node is.
    pub version: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum QueryResult {
    Status(StatusReport),
    State(Box<crate::state::ClusterState>),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RpcResponse {
    AppendEntries(AppendEntriesResponse<NodeId>),
    InstallSnapshot(InstallSnapshotResponse<NodeId>),
    Vote(VoteResponse<NodeId>),
    /// The forwarded write's outcome, or why it could not be applied here.
    Forward(std::result::Result<StateResponse, String>),
    Query(Box<QueryResult>),
    Pong,
    /// The peer understood the frame but could not serve it, for instance
    /// because it is not the leader.
    Error(String),
}

pub type Connection = Framed<TcpStream, LengthDelimitedCodec>;

/// Wrap a socket in the framing both ends use.
pub fn frame(stream: TcpStream) -> Connection {
    // Nagle would add latency to the small heartbeat frames for no benefit.
    let _ = stream.set_nodelay(true);
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME)
        .new_framed(stream)
}

pub fn encode<T: Serialize>(value: &T) -> io::Result<bytes::Bytes> {
    let raw = serde_json::to_vec(value).map_err(io::Error::other)?;
    Ok(bytes::Bytes::from(raw))
}

pub fn decode<T: for<'de> Deserialize<'de>>(raw: &[u8]) -> io::Result<T> {
    serde_json::from_slice(raw).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        let request = RpcRequest::Forward(StateRequest::NoOp);
        let raw = encode(&request).expect("encode");
        let decoded: RpcRequest = decode(&raw).expect("decode");
        assert!(matches!(decoded, RpcRequest::Forward(StateRequest::NoOp)));
    }

    #[test]
    fn forward_errors_survive_the_wire() {
        let response = RpcResponse::Forward(Err("not leader".into()));
        let raw = encode(&response).expect("encode");
        match decode::<RpcResponse>(&raw).expect("decode") {
            RpcResponse::Forward(Err(msg)) => assert_eq!(msg, "not leader"),
            other => panic!("unexpected {other:?}"),
        }
    }
}
