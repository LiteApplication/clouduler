//! What the socket answers from. A trait so the HTTP layer is testable without
//! a docker daemon, and so the cluster-wide backend can sit in front of the
//! local one.

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde_json::Value;

/// Why a backend could not answer.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Other(String),
}

pub type BackendResult<T> = Result<T, BackendError>;

pub trait ImageBackend: Send + Sync + 'static {
    /// `GET /images/{name}/json`.
    fn inspect<'a>(&'a self, name: &'a str) -> BoxFuture<'a, BackendResult<Value>>;
    /// `GET /images/{name}/history`.
    fn history<'a>(&'a self, name: &'a str) -> BoxFuture<'a, BackendResult<Value>>;
    /// `GET /images/json`.
    fn list(&self) -> BoxFuture<'_, BackendResult<Value>>;
    /// `GET /images/{name}/get`: a `docker save` tar, streamed. Resolved
    /// before the first byte so a missing image is a 404, not a cut stream.
    fn save<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, BackendResult<BoxStream<'static, BackendResult<Bytes>>>>;
    /// `POST /images/create`: make the image available.
    fn pull<'a>(&'a self, reference: &'a str) -> BoxFuture<'a, BackendResult<()>>;
}

/// The node's own docker daemon.
pub struct LocalBackend {
    docker: cl_docker::Docker,
}

impl LocalBackend {
    pub fn new(docker: cl_docker::Docker) -> Self {
        Self { docker }
    }
}

impl From<cl_docker::DockerError> for BackendError {
    fn from(e: cl_docker::DockerError) -> Self {
        match e {
            cl_docker::DockerError::NotFound(m) => BackendError::NotFound(m),
            other => BackendError::Other(other.to_string()),
        }
    }
}

impl ImageBackend for LocalBackend {
    fn inspect<'a>(&'a self, name: &'a str) -> BoxFuture<'a, BackendResult<Value>> {
        Box::pin(async move { Ok(self.docker.inspect_image_json(name).await?) })
    }

    fn history<'a>(&'a self, name: &'a str) -> BoxFuture<'a, BackendResult<Value>> {
        Box::pin(async move { Ok(self.docker.image_history_json(name).await?) })
    }

    fn list(&self) -> BoxFuture<'_, BackendResult<Value>> {
        Box::pin(async move { Ok(self.docker.list_images_json().await?) })
    }

    fn save<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, BackendResult<BoxStream<'static, BackendResult<Bytes>>>> {
        use futures::StreamExt;
        Box::pin(async move {
            // The engine answers 200 and then fails mid-stream for an unknown
            // image, so check it exists first to give a clean 404.
            self.docker.inspect_image_json(name).await?;
            Ok(self
                .docker
                .save_image(name)
                .map(|chunk| chunk.map_err(BackendError::from))
                .boxed())
        })
    }

    fn pull<'a>(&'a self, reference: &'a str) -> BoxFuture<'a, BackendResult<()>> {
        Box::pin(async move {
            self.docker.pull(reference).await?;
            Ok(())
        })
    }
}
