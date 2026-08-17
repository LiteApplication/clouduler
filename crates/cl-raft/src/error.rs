use std::path::PathBuf;

use crate::ids::NodeId;

pub type Result<T> = std::result::Result<T, ClusterError>;

#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    #[error("cannot use raft data directory {path}: {source}")]
    Storage {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("raft storage: {0}")]
    Raft(#[from] crate::types::StorageError),

    #[error("invalid raft config: {0}")]
    Config(#[from] openraft::ConfigError),

    #[error("raft: {0}")]
    Fatal(#[from] openraft::error::Fatal<NodeId>),

    #[error("cluster is already initialized: {0}")]
    Initialize(String),

    #[error("membership change: {0}")]
    Membership(String),

    #[error("write rejected: {0}")]
    Write(String),

    /// No leader is known, which means there is no quorum right now.
    #[error("no leader reachable")]
    NoLeader,

    #[error("leader {leader} rejected the forwarded write: {msg}")]
    Forward { leader: NodeId, msg: String },

    #[error("shutdown: {0}")]
    Shutdown(String),
}

// openraft's errors are generic over the operation, so there is no single type
// to convert from. Flattening them to a message at the boundary keeps the
// signatures of this crate readable.
impl ClusterError {
    pub fn initialize(e: impl std::fmt::Display) -> Self {
        ClusterError::Initialize(e.to_string())
    }

    pub fn membership(e: impl std::fmt::Display) -> Self {
        ClusterError::Membership(e.to_string())
    }

    pub fn write(e: impl std::fmt::Display) -> Self {
        ClusterError::Write(e.to_string())
    }
}
