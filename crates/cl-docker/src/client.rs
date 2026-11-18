//! Connection to the local docker daemon.

use crate::error::{DockerError, Result};

/// The docker daemon's unix socket on the host, matching what `Docker::connect`
/// uses. Bind-mounted into containers asking for `x-clouduler.docker_socket`.
pub const DOCKER_SOCKET_PATH: &str = "/var/run/docker.sock";

/// Thin wrapper around a connected bollard client; other modules add methods via
/// `impl Docker` blocks.
#[derive(Clone)]
pub struct Docker {
    pub(crate) inner: bollard::Docker,
}

impl Docker {
    pub fn connect() -> Result<Self> {
        let inner =
            bollard::Docker::connect_with_socket_defaults().map_err(DockerError::Connect)?;
        Ok(Self { inner })
    }

    /// Health check, for failing fast at startup on a dead daemon.
    pub async fn ping(&self) -> Result<()> {
        self.inner.ping().await.map_err(DockerError::Connect)?;
        Ok(())
    }

    /// Names of the container runtimes the daemon has registered (`runc`,
    /// `nvidia`, ...). The NVIDIA container toolkit shows up as `nvidia`.
    pub async fn runtimes(&self) -> Result<Vec<String>> {
        let info = self.inner.info().await.map_err(DockerError::Connect)?;
        let mut names: Vec<String> = info
            .runtimes
            .map(|r| r.into_keys().collect())
            .unwrap_or_default();
        names.sort();
        Ok(names)
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
