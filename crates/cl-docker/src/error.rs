//! Error types for the docker client.

use thiserror::Error;

/// Result alias used throughout this crate.
pub type Result<T> = std::result::Result<T, DockerError>;

/// Errors talking to the local docker daemon.
#[derive(Debug, Error)]
pub enum DockerError {
    /// The daemon could not be reached at all (socket missing, permission denied, etc).
    #[error("could not connect to docker daemon")]
    Connect(#[source] bollard::errors::Error),

    /// The daemon responded with a normal 404. This is an expected condition for
    /// the reconcile loop (container already gone), not something to log as an error.
    #[error("not found: {0}")]
    NotFound(String),

    /// Any other error the daemon returned.
    #[error("docker api error")]
    Api(#[source] bollard::errors::Error),

    /// An image reference couldn't be parsed or resolved.
    #[error("invalid image reference: {0}")]
    InvalidImageRef(String),
}

impl DockerError {
    /// True when this is a benign not-found condition rather than a real failure.
    pub fn is_not_found(&self) -> bool {
        matches!(self, DockerError::NotFound(_))
    }
}

/// Turn a raw bollard error into ours, pulling 404s out as `NotFound` so callers
/// can match on them without inspecting bollard's error type directly.
pub(crate) fn classify(err: bollard::errors::Error) -> DockerError {
    if let bollard::errors::Error::DockerResponseServerError {
        status_code: 404,
        message,
    } = &err
    {
        return DockerError::NotFound(message.clone());
    }
    DockerError::Api(err)
}

impl From<bollard::errors::Error> for DockerError {
    fn from(err: bollard::errors::Error) -> Self {
        classify(err)
    }
}
