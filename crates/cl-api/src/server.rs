//! Builds the axum `Router` and serves it over HTTPS.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use cl_config::ClusterConfig;
use cl_docker::Docker;
use cl_raft::ClusterNode;
use cl_secret::ClusterKey;
use tokio::sync::{Mutex, RwLock};
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::auth::oidc::OidcAuthenticator;
use crate::auth::session::SessionStore;
use crate::config_edit::PendingEdit;
use crate::error::ApiError;
use crate::routes;
use crate::ui;

/// Everything a handler might need, cheap to clone since it is all shared
/// state behind `Arc` or an async lock.
#[derive(Clone)]
pub struct AppState {
    pub cluster: Arc<ClusterNode>,
    /// Empty when this node has no docker daemon to talk to. The api still
    /// serves everything else: a node whose docker is down still votes, still
    /// holds replicated state and still has to be reachable to be fixed.
    ///
    /// Shared rather than owned because the daemon waits for dockerd on its
    /// own schedule; the api comes up first and picks the client up whenever
    /// it lands.
    pub docker: Arc<RwLock<Option<Docker>>>,
    /// Name of the node this api instance is running on. Used for the local
    /// half of node detail (resource usage, daemon logs) that has no RPC path
    /// to another node's process.
    pub node_name: String,
    pub config_path: PathBuf,
    pub cluster_config: Arc<RwLock<ClusterConfig>>,
    pub key_path: PathBuf,
    pub key: Arc<RwLock<Option<ClusterKey>>>,
    pub sessions: Arc<SessionStore>,
    pub oidc: Option<Arc<OidcAuthenticator>>,
    pub pending_edit: Arc<Mutex<Option<PendingEdit>>>,
    /// Fixed point `config_edit`'s pure clock counts from.
    pub started_at: Instant,
    pub session_ttl: Duration,
    pub rollback_after: Duration,
}

impl AppState {
    /// Duration since `started_at`, the clock `config_edit::PendingEdit` uses.
    pub fn clock(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// The docker client, or a 503 naming why there is none.
    pub async fn docker(&self) -> Result<Docker, ApiError> {
        self.docker.read().await.clone().ok_or_else(|| {
            ApiError::Unavailable(
                "this node has no docker daemon, so it cannot answer for containers".to_string(),
            )
        })
    }
}

/// What the daemon passes in to stand up the api.
pub struct ApiServerOptions {
    pub cluster: Arc<ClusterNode>,
    pub docker: Arc<RwLock<Option<Docker>>>,
    pub node_name: String,
    pub config_path: PathBuf,
    pub cluster_config: ClusterConfig,
    pub key_path: PathBuf,
    pub key: Option<ClusterKey>,
    pub tls_cert_pem: Vec<u8>,
    pub tls_key_pem: Vec<u8>,
    pub listen_addr: SocketAddr,
}

pub struct ApiServer {
    router: Router,
    listen_addr: SocketAddr,
    tls_cert_pem: Vec<u8>,
    tls_key_pem: Vec<u8>,
}

impl ApiServer {
    pub async fn new(options: ApiServerOptions) -> Result<Self, ApiError> {
        let session_ttl = options.cluster_config.auth.session_ttl;
        let oidc = match &options.cluster_config.auth.oidc {
            Some(cfg) => {
                let client_secret = match &cfg.client_secret_file {
                    Some(path) => Some(
                        std::fs::read_to_string(path)
                            .map_err(|e| ApiError::Internal(format!("reading {}: {e}", path.display())))?
                            .trim()
                            .to_string(),
                    ),
                    None => None,
                };
                let redirect_url = format!(
                    "https://{}/auth/oidc/callback",
                    options.cluster_config.web_fqdn()
                );
                Some(Arc::new(OidcAuthenticator::new(crate::auth::oidc::OidcSettings {
                    issuer: cfg.issuer.clone(),
                    client_id: cfg.client_id.clone(),
                    client_secret,
                    redirect_url,
                    scopes: cfg.scopes.clone(),
                    admin_claim: cfg.admin_claim.clone(),
                    admin_value: cfg.admin_value.clone(),
                })?))
            }
            None => None,
        };

        let state = AppState {
            cluster: options.cluster,
            docker: options.docker,
            node_name: options.node_name,
            config_path: options.config_path,
            cluster_config: Arc::new(RwLock::new(options.cluster_config)),
            key_path: options.key_path,
            key: Arc::new(RwLock::new(options.key)),
            sessions: Arc::new(SessionStore::new()),
            oidc,
            pending_edit: Arc::new(Mutex::new(None)),
            started_at: Instant::now(),
            session_ttl,
            rollback_after: crate::config_edit::DEFAULT_ROLLBACK_AFTER,
        };

        // First launch of a fresh cluster: mint an api key, since it is the
        // only way in until either OIDC is configured or a key already exists.
        // Backgrounded because at this point the cluster usually has no leader
        // yet and the write would have nowhere to go.
        routes::auth_keys::spawn_bootstrap_first_key(state.clone());

        Ok(Self {
            router: build_router(state),
            listen_addr: options.listen_addr,
            tls_cert_pem: options.tls_cert_pem,
            tls_key_pem: options.tls_key_pem,
        })
    }

    pub async fn serve(self) -> Result<(), ApiError> {
        let config = axum_server::tls_rustls::RustlsConfig::from_pem(self.tls_cert_pem, self.tls_key_pem)
            .await
            .map_err(|e| ApiError::Internal(format!("loading tls config: {e}")))?;
        axum_server::bind_rustls(self.listen_addr, config)
            .serve(self.router.into_make_service())
            .await
            .map_err(|e| ApiError::Internal(format!("serving: {e}")))
    }
}

/// A router with no TLS wrapper, driven with `tower::ServiceExt::oneshot` in
/// tests. Public so `src/tests.rs` can build one from a hand-assembled
/// `AppState` without going through `ApiServer::new`'s OIDC discovery and
/// bootstrap key submission.
#[doc(hidden)]
pub fn build_router(state: AppState) -> Router {
    let api = routes::router()
        .route_layer(middleware::from_fn_with_state(state.clone(), redirect_non_leader));

    Router::new()
        .merge(api)
        .fallback(get(ui::serve))
        .layer(CompressionLayer::new())
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Only the leader has a fully caught up view of the cluster and can accept
/// writes, so every `/api` and `/auth` route bounces elsewhere when this node
/// is not it. The ui's static files are served regardless, so a client that
/// lands on a follower still gets a working page that then retries the api
/// call against the address in `Location`.
async fn redirect_non_leader(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Result<Response, ApiError> {
    if state.cluster.is_leader() {
        return Ok(next.run(request).await);
    }

    // Writes go to the leader, reads answer anywhere.
    //
    // A write has to be ordered against every other write, so it belongs on the
    // leader. A read does not, and insisting on the leader for reads makes some
    // things simply unreachable: live container state, resource usage and
    // daemon logs exist only on the node they describe, and there is no rpc
    // path for the leader to fetch another node's. Bouncing a read at that node
    // to the leader means the answer can never be obtained at all.
    //
    // The trade is that a lagging follower can answer with slightly stale
    // replicated state. `/api/status` carries `state_version` so a caller that
    // cares can tell.
    if request.method() == axum::http::Method::GET {
        return Ok(next.run(request).await);
    }

    // Auth's own login and callback routes are POSTs that have to work
    // everywhere, or a client stuck on a follower can never log in to discover
    // who the leader is.
    let path = request.uri().path();
    if path.starts_with("/auth/oidc/") {
        return Ok(next.run(request).await);
    }

    Err(ApiError::NotLeader {
        leader: leader_location(&state).await,
    })
}

/// Best-effort address of the current leader, built from its internal rpc
/// address plus the configured web port. The cluster does not track a
/// separate api-facing address per node, so this is the closest thing to it.
async fn leader_location(state: &AppState) -> Option<String> {
    let quorum = state.cluster.quorum();
    let leader_id = quorum.leader()?;
    let cluster_state = state.cluster.state().await;
    let (_, node) = cluster_state
        .nodes
        .iter()
        .find(|(name, _)| cl_raft::node_id(name) == leader_id)?;
    let port = state.cluster_config.read().await.web.port;
    Some(format!("https://{}:{port}/", node.rpc_addr.ip()))
}
