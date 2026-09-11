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

    /// Any other error the daemon returned. The daemon's own message is part
    /// of the display text: callers log the error rather than walking its
    /// source chain, and "docker api error" on its own says nothing about
    /// what docker actually refused.
    #[error("docker api error: {0}")]
    Api(#[source] bollard::errors::Error),

    /// The registry answered 429. Distinct from `Api` because it is a
    /// "try again shortly", not a failure: the answer exists, we asked too
    /// fast. Registries throttle bursts even well under their published
    /// per-minute allowance, so this is routine when checking many images.
    #[error("registry is rate limiting: {0}")]
    RateLimited(String),

    /// An image reference couldn't be parsed or resolved.
    #[error("invalid image reference: {0}")]
    InvalidImageRef(String),

    /// Something outside the docker API proper failed: shelling out to
    /// `nsenter` for `Docker::set_default_route`, mainly.
    #[error("{0}")]
    Other(String),
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
        status_code,
        message,
    } = &err
    {
        match status_code {
            404 => return DockerError::NotFound(message.clone()),
            // The registry is throttling, not refusing. Worth its own variant
            // so a caller can back off and retry instead of reporting an
            // image as unknown — see `Docker::registry_digest`.
            429 => return DockerError::RateLimited(message.clone()),
            _ => {}
        }
    }
    DockerError::Api(err)
}

impl From<bollard::errors::Error> for DockerError {
    fn from(err: bollard::errors::Error) -> Self {
        classify(err)
    }
}
