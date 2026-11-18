//! Node to node RPC: length prefixed json over plain TCP on the internal
//! network (trusted, so no TLS; keeps the channel cheap enough for drive sync
//! and proxy backhaul).

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

/// Refuse larger frames so a corrupt length prefix cannot cause huge allocations.
const MAX_FRAME: usize = 64 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub enum RpcRequest {
    AppendEntries(AppendEntriesRequest<TypeConfig>),
    InstallSnapshot(InstallSnapshotRequest<TypeConfig>),
    Vote(VoteRequest<NodeId>),
    Forward(StateRequest),
    Query(Query),
    Ping,
}

/// Reads come from the asked node's own applied state and may lag the leader;
/// anything needing linearizability goes through a write.
#[derive(Debug, Serialize, Deserialize)]
pub enum Query {
    Status,
    State {
        /// Secrets and private keys are blanked unless set (only the cli's backup paths).
        include_secrets: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    pub node: String,
    pub id: NodeId,
    pub leader: Option<NodeId>,
    pub is_leader: bool,
    pub has_quorum: bool,
    pub voters: usize,
    pub reachable: usize,
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
    Forward(std::result::Result<StateResponse, String>),
    Query(Box<QueryResult>),
    Pong,
    /// The peer could not serve the frame, e.g. it is not the leader.
    Error(String),
}

pub type Connection = Framed<TcpStream, LengthDelimitedCodec>;

pub fn frame(stream: TcpStream) -> Connection {
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
