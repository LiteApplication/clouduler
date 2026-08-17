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
    pub docker: Docker,
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
}

/// What the daemon passes in to stand up the api.
pub struct ApiServerOptions {
    pub cluster: Arc<ClusterNode>,
    pub docker: Docker,
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
        // only way in until either OIDC is configured or a key already
        // exists.
        routes::auth_keys::bootstrap_first_key(&state).await;

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

    // Auth's own login/callback routes and /auth/me have to work everywhere,
    // otherwise a client stuck on a follower can never even discover who the
    // leader is via a normal api call.
    let path = request.uri().path();
    if path == "/auth/me" || path.starts_with("/auth/oidc/") {
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
