//! Serves clouduler's read-only docker API socket (`cl-dockerapi`) on this
//! node, for stacks that mount it with `docker_socket: { mode: clouduler }`.
//!
//! Images are answered cluster-wide: this node's docker first, then every
//! other reachable node through the signed `/api/images/internal/*` routes
//! (`cl-api::routes::images`). A scanner can therefore name any image a
//! container in the cluster is running without caring which node holds it.
//!
//! Always on: a container that bind-mounts a socket path nobody is listening
//! on makes docker create a directory there instead, which then blocks the
//! real socket from ever being bound.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use cl_config::ClusterConfig;
use cl_dockerapi::{BackendError, BackendResult, ImageBackend, LocalBackend};
use cl_raft::node::ClusterNode;
use cl_secret::ClusterKey;
use cl_update::internal_client;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::{watch, RwLock};
use tracing::{debug, error, warn};

type SharedDocker = Arc<RwLock<Option<cl_docker::Docker>>>;

/// A peer that cannot answer a small read this fast is treated as not having
/// the image, so one slow node never stalls a scan.
const PEER_READ_TIMEOUT: Duration = Duration::from_secs(10);

pub struct ClusterBackend {
    pub docker: SharedDocker,
    pub node_name: String,
    pub cluster: Arc<ClusterNode>,
    pub config: watch::Receiver<Arc<ClusterConfig>>,
    pub key: watch::Receiver<Option<Arc<ClusterKey>>>,
    pub http: reqwest::Client,
}

/// How a peer read ended.
enum Peer<T> {
    Found(T),
    /// Answered, and does not hold it.
    Missing,
    /// Could not be asked, or answered something unexpected.
    Failed,
}

impl ClusterBackend {
    /// The node's docker handle as the daemon holds it: absent until docker is
    /// reachable, replaced when it reconnects. Each request picks up the
    /// current one, so a bounced dockerd does not need the socket restarted.
    async fn local(&self) -> BackendResult<LocalBackend> {
        self.docker
            .read()
            .await
            .clone()
            .map(LocalBackend::new)
            .ok_or_else(|| BackendError::Other("docker is not reachable on this node".into()))
    }

    /// Every other node that is not known to be down, with its api origin.
    async fn peers(&self) -> Vec<(String, String)> {
        let state = self.cluster.state().await;
        let cfg = self.config.borrow().clone();
        state
            .nodes
            .keys()
            .filter(|n| **n != self.node_name && !state.unreachable.contains(*n))
            .filter_map(|n| Some((n.clone(), cl_update::peers::base_url(&cfg, n)?)))
            .collect()
    }

    fn signed_get(&self, base: &str, path_and_query: &str) -> Option<reqwest::RequestBuilder> {
        let key = self.key.borrow().clone()?;
        Some(internal_client::authenticate(
            self.http.get(format!("{base}{path_and_query}")),
            &key,
            "GET",
            path_and_query,
            b"",
        ))
    }

    /// `GET /api/images/internal/<op>?name=<name>` on a peer, decoded as json.
    async fn peer_json(&self, base: &str, op: &str, name: Option<&str>) -> Peer<Value> {
        let path = match name {
            Some(name) => format!("/api/images/internal/{op}?{}", encode_name(name)),
            None => format!("/api/images/internal/{op}"),
        };
        let Some(request) = self.signed_get(base, &path) else {
            return Peer::Failed;
        };
        match request.timeout(PEER_READ_TIMEOUT).send().await {
            Ok(resp) if resp.status().is_success() => match resp.json().await {
                Ok(v) => Peer::Found(v),
                Err(_) => Peer::Failed,
            },
            Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => Peer::Missing,
            Ok(resp) => {
                debug!(%base, status = %resp.status(), "peer refused an image read");
                Peer::Failed
            }
            Err(e) => {
                debug!(%base, error = %e, "peer unreachable for an image read");
                Peer::Failed
            }
        }
    }

    /// The first peer that holds `name`, with its answer to `op`.
    async fn from_any_peer(&self, op: &str, name: &str) -> Option<Value> {
        for (node, base) in self.peers().await {
            if let Peer::Found(v) = self.peer_json(&base, op, Some(name)).await {
                debug!(%node, %name, op, "image answered by a peer");
                return Some(v);
            }
        }
        None
    }

    async fn holder_of(&self, name: &str) -> Option<String> {
        for (_, base) in self.peers().await {
            if let Peer::Found(_) = self.peer_json(&base, "inspect", Some(name)).await {
                return Some(base);
            }
        }
        None
    }
}

fn encode_name(name: &str) -> String {
    serde_urlencoded::to_string([("name", name)]).expect("a string pair always encodes")
}

fn not_found(name: &str) -> BackendError {
    BackendError::NotFound(format!("No such image: {name}"))
}

impl ImageBackend for ClusterBackend {
    fn inspect<'a>(&'a self, name: &'a str) -> BoxFuture<'a, BackendResult<Value>> {
        Box::pin(async move {
            match self.local().await?.inspect(name).await {
                Err(BackendError::NotFound(_)) => self
                    .from_any_peer("inspect", name)
                    .await
                    .ok_or_else(|| not_found(name)),
                other => other,
            }
        })
    }

    fn history<'a>(&'a self, name: &'a str) -> BoxFuture<'a, BackendResult<Value>> {
        Box::pin(async move {
            match self.local().await?.history(name).await {
                Err(BackendError::NotFound(_)) => self
                    .from_any_peer("history", name)
                    .await
                    .ok_or_else(|| not_found(name)),
                other => other,
            }
        })
    }

    fn list(&self) -> BoxFuture<'_, BackendResult<Value>> {
        Box::pin(async move {
            // Local images first; a peer's only add the ones not already listed.
            let mut images = match self.local().await?.list().await? {
                Value::Array(items) => items,
                _ => Vec::new(),
            };
            let peers = self.peers().await;
            let answers = futures::future::join_all(
                peers
                    .iter()
                    .map(|(_, base)| self.peer_json(base, "list", None)),
            )
            .await;
            for answer in answers {
                let Peer::Found(Value::Array(items)) = answer else {
                    continue;
                };
                for item in items {
                    let id = item.get("Id");
                    if !images.iter().any(|have| have.get("Id") == id) {
                        images.push(item);
                    }
                }
            }
            Ok(Value::Array(images))
        })
    }

    fn save<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, BackendResult<BoxStream<'static, BackendResult<Bytes>>>> {
        Box::pin(async move {
            match self.local().await?.save(name).await {
                Err(BackendError::NotFound(_)) => {}
                other => return other,
            }
            let base = self.holder_of(name).await.ok_or_else(|| not_found(name))?;
            let path = format!("/api/images/internal/save?{}", encode_name(name));
            let request = self
                .signed_get(&base, &path)
                .ok_or_else(|| BackendError::Other("this node holds no cluster key".into()))?;
            // No overall timeout: an image is large and the body streams. The
            // connect/headers phase still has to answer promptly.
            let resp = tokio::time::timeout(PEER_READ_TIMEOUT, request.send())
                .await
                .map_err(|_| BackendError::Other(format!("timed out asking for {name}")))?
                .map_err(|e| BackendError::Other(format!("asking a peer for {name}: {e}")))?;
            if !resp.status().is_success() {
                return Err(BackendError::Other(format!(
                    "a peer answered {} for {name}",
                    resp.status()
                )));
            }
            Ok(resp
                .bytes_stream()
                .map(|chunk| chunk.map_err(|e| BackendError::Other(e.to_string())))
                .boxed())
        })
    }

    fn pull<'a>(&'a self, reference: &'a str) -> BoxFuture<'a, BackendResult<()>> {
        Box::pin(async move {
            // A scanner pulls when it finds nothing; a peer already holding the
            // image makes that unnecessary.
            if self.inspect(reference).await.is_ok() {
                return Ok(());
            }
            self.local().await?.pull(reference).await
        })
    }
}

pub fn start(socket: PathBuf, backend: ClusterBackend) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Aborted at shutdown, which drops the future; the socket file is
        // replaced on the next bind either way.
        match cl_dockerapi::serve(&socket, Arc::new(backend), std::future::pending()).await {
            Ok(()) => warn!("docker api socket stopped"),
            Err(e) => error!(socket = %socket.display(), error = %e, "docker api socket failed"),
        }
    })
}
