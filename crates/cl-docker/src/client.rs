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
    /// Successful image inspections by image id. An id is content-addressed, so
    /// its inspection never changes, and each inspect makes containerd compute
    /// snapshot usage, which is very slow on a loaded node.
    pub(crate) image_cache: ImageCache,
}

/// Shared between clones of a [`Docker`]. Only successes are stored.
#[derive(Clone, Default)]
pub(crate) struct ImageCache(
    std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, std::sync::Arc<bollard::models::ImageInspect>>,
        >,
    >,
);

/// Generous for one node's images; cleared wholesale when exceeded.
const IMAGE_CACHE_LIMIT: usize = 1024;

impl ImageCache {
    pub(crate) fn get(&self, id: &str) -> Option<std::sync::Arc<bollard::models::ImageInspect>> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    pub(crate) fn remove(&self, id: &str) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
    }

    pub(crate) fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    pub(crate) fn insert(&self, id: &str, info: std::sync::Arc<bollard::models::ImageInspect>) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= IMAGE_CACHE_LIMIT {
            map.clear();
        }
        map.insert(id.to_string(), info);
    }
}

impl Docker {
    pub fn connect() -> Result<Self> {
        let inner =
            bollard::Docker::connect_with_socket_defaults().map_err(DockerError::Connect)?;
        Ok(Self {
            inner,
            image_cache: ImageCache::default(),
        })
    }

    /// Forget every cached image inspection, so the next lookup asks docker again.
    pub fn clear_image_cache(&self) {
        self.image_cache.clear();
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

    #[test]
    fn image_cache_returns_what_was_stored() {
        let cache = ImageCache::default();
        assert!(cache.get("sha256:a").is_none());
        let info = std::sync::Arc::new(bollard::models::ImageInspect {
            id: Some("sha256:a".into()),
            ..Default::default()
        });
        cache.insert("sha256:a", info);
        let hit = cache.get("sha256:a").expect("cached");
        assert_eq!(hit.id.as_deref(), Some("sha256:a"));
        assert!(cache.get("sha256:b").is_none());
    }

    #[test]
    fn image_cache_remove_and_clear() {
        let cache = ImageCache::default();
        let info = std::sync::Arc::new(bollard::models::ImageInspect::default());
        cache.insert("a", info.clone());
        cache.insert("b", info);
        cache.remove("a");
        assert!(cache.get("a").is_none() && cache.get("b").is_some());
        cache.clear();
        assert!(cache.get("b").is_none());
    }

    #[tokio::test]
    #[ignore = "needs a running docker daemon"]
    async fn connect_and_ping_local_daemon() {
        let docker = Docker::connect().expect("connect to docker socket");
        docker.ping().await.expect("ping daemon");
    }
}
