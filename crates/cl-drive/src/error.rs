//! Error types for drives, sync and migration.

use std::path::PathBuf;

use cl_raft::VolumeId;

pub type Result<T> = std::result::Result<T, DriveError>;

#[derive(Debug, thiserror::Error)]
pub enum DriveError {
    #[error("node {node} does not implement drive {drive}")]
    NotImplemented { drive: String, node: String },

    #[error("volume {volume} not found")]
    VolumeNotFound { volume: VolumeId },

    /// Resolved outside the drive root, e.g. via `..` or an absolute path in a name.
    #[error("path {path} escapes the drive root")]
    PathEscape { path: String },

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Connect failure, protocol mismatch, or the peer closing early.
    #[error("transfer failed: {message}")]
    Transfer { message: String },

    /// The source has little or nothing where the replica has data. Nothing was sent.
    #[error("refusing to sync {volume}: {reason}")]
    DestructiveSync { volume: VolumeId, reason: String },

    #[error("verification failed: {message}")]
    Verify { message: String },

    #[error("config error: {0}")]
    Config(String),
}
