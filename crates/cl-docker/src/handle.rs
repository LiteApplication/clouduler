use std::sync::Arc;

use tokio::sync::RwLock;

use crate::Docker;

/// The daemon's possibly-absent connection to the local Docker engine, shared
/// between tasks. It is retried in the background, so the engine can appear or
/// vanish at runtime.
#[derive(Clone, Default)]
pub struct DockerHandle(Arc<RwLock<Option<Docker>>>);

impl DockerHandle {
    pub fn new(docker: Option<Docker>) -> Self {
        Self(Arc::new(RwLock::new(docker)))
    }

    pub async fn get(&self) -> Option<Docker> {
        self.0.read().await.clone()
    }

    pub async fn is_connected(&self) -> bool {
        self.0.read().await.is_some()
    }

    pub async fn set(&self, docker: Option<Docker>) {
        *self.0.write().await = docker;
    }
}
