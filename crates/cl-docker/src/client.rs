//! Connection to the local docker daemon.

use crate::error::{DockerError, Result};

/// Thin wrapper around a connected bollard client. All other modules in this
/// crate add methods to this type via `impl Docker` blocks in their own files.
#[derive(Clone)]
pub struct Docker {
    pub(crate) inner: bollard::Docker,
}

impl Docker {
    /// Connect to the docker daemon over the local unix socket.
    pub fn connect() -> Result<Self> {
        let inner =
            bollard::Docker::connect_with_socket_defaults().map_err(DockerError::Connect)?;
        Ok(Self { inner })
    }

    /// Health check. Callers should use this at startup to fail fast rather than
    /// discover a dead daemon on the first container operation.
    pub async fn ping(&self) -> Result<()> {
        self.inner.ping().await.map_err(DockerError::Connect)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "needs a running docker daemon"]
    async fn connect_and_ping_local_daemon() {
        let docker = Docker::connect().expect("connect to docker socket");
        docker.ping().await.expect("ping daemon");
    }
}
