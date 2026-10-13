//! Error types for drives, sync and migration.

use std::path::PathBuf;

use cl_raft::VolumeId;

pub type Result<T> = std::result::Result<T, DriveError>;

#[derive(Debug, thiserror::Error)]
pub enum DriveError {
    /// The node was asked to act on a drive it has no local directory for.
    #[error("node {node} does not implement drive {drive}")]
    NotImplemented { drive: String, node: String },

    #[error("volume {volume} not found")]
    VolumeNotFound { volume: VolumeId },

    /// A volume or file path resolved outside the drive root, e.g. via `..`
    /// or an absolute path smuggled in through a name.
    #[error("path {path} escapes the drive root")]
    PathEscape { path: String },

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Something went wrong moving bytes over the wire: connect failure,
    /// protocol mismatch, or the peer closing early.
    #[error("transfer failed: {message}")]
    Transfer { message: String },

    /// The sync would have wiped (most of) a replica's data from a source
    /// that has little or nothing. Nothing was sent.
    #[error("refusing to sync {volume}: {reason}")]
    DestructiveSync { volume: VolumeId, reason: String },

    /// Post-transfer manifests did not match.
    #[error("verification failed: {message}")]
    Verify { message: String },

    /// Cluster config did not describe what the caller assumed it would.
    #[error("config error: {0}")]
    Config(String),
}
