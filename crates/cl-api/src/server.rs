//! Builds the axum `Router` and serves it over HTTPS.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
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
    /// Where a self-update artifact this node built or fetched is staged,
    /// waiting for the internal restart route to install it.
    pub self_update_artifact_path: PathBuf,
    /// Fires the daemon's clean shutdown path, the same one `SIGINT` drives,
    /// so the internal restart route can trigger it programmatically after
    /// swapping in a new binary.
    pub shutdown: tokio::sync::watch::Sender<()>,
    /// For forwarding a read to the node that actually holds the answer
    /// (container inspection, logs, stats, resource usage). Accepts invalid
    /// certs for the same reason `cl-update`'s internal client does: peers
    /// are authenticated by the HMAC header, not TLS trust, since every
    /// node's certificate is self signed until ACME issues a real one.
    pub http: reqwest::Client,
    /// SHA-256 fingerprint of the certificate this instance is actually
    /// serving over TLS, in the same colon-hex form `clouduler init` prints
    /// and pins with `--tls-fingerprint`. Computed once at startup from the
    /// same bytes handed to the TLS listener, so it always matches what a
    /// joining node sees, self signed or issued.
    pub tls_fingerprint: String,
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
    /// Plain HTTP listener the reverse proxy forwards dashboard traffic to.
    /// `None` leaves it off. Always a loopback address; see `WebConfig`.
    pub plain_listen_addr: Option<SocketAddr>,
    pub self_update_artifact_path: PathBuf,
    pub shutdown: tokio::sync::watch::Sender<()>,
    pub http: reqwest::Client,
    /// This node's own asymmetric keypair, used only to unwrap a cluster key
    /// delivered automatically through raft. See `routes::key_distribution`.
    pub node_keypair: Option<cl_secret::NodeKeypair>,
}

pub struct ApiServer {
    router: Router,
    listen_addr: SocketAddr,
    plain_listen_addr: Option<SocketAddr>,
    tls_cert_pem: Vec<u8>,
    tls_key_pem: Vec<u8>,
}

/// SHA-256 fingerprint of the leaf certificate in a PEM chain, colon-hex
/// formatted to match `clouduler init`'s `join::fingerprint`.
fn leaf_fingerprint(chain_pem: &[u8]) -> Result<String, ApiError> {
    use base64::Engine;
    use sha2::{Digest, Sha256};

    let text = std::str::from_utf8(chain_pem)
        .map_err(|e| ApiError::Internal(format!("tls certificate is not valid utf8: {e}")))?;
    let body = text
        .split("-----BEGIN CERTIFICATE-----")
        .nth(1)
        .and_then(|rest| rest.split("-----END CERTIFICATE-----").next())
        .ok_or_else(|| ApiError::Internal("no certificate found in tls chain".to_string()))?;
    let der = base64::engine::general_purpose::STANDARD
        .decode(body.split_whitespace().collect::<String>())
        .map_err(|e| ApiError::Internal(format!("tls certificate is not valid base64: {e}")))?;
    let digest = Sha256::digest(&der);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect::<Vec<_>>().join(":"))
}

impl ApiServer {
    pub async fn new(options: ApiServerOptions) -> Result<Self, ApiError> {
        let session_ttl = options.cluster_config.auth.session_ttl;
        let tls_fingerprint = leaf_fingerprint(&options.tls_cert_pem)?;
        let key = Arc::new(RwLock::new(options.key));
        let oidc = match &options.cluster_config.auth.oidc {
            Some(cfg) => {
                let redirect_url = format!(
                    "https://{}/auth/oidc/callback",
                    options.cluster_config.web_fqdn()
                );
                Some(Arc::new(OidcAuthenticator::new(
                    crate::auth::oidc::OidcSettings {
                        issuer: cfg.issuer.clone(),
                        client_id: cfg.client_id.clone(),
                        redirect_url,
                        scopes: cfg.scopes.clone(),
                        admin_claim: cfg.admin_claim.clone(),
                        admin_value: cfg.admin_value.clone(),
                    },
                    options.cluster.clone(),
                    key.clone(),
                )?))
            }
            None => None,
        };

        let node_keypair = options.node_keypair;
        let state = AppState {
            cluster: options.cluster,
            docker: options.docker,
            node_name: options.node_name,
            config_path: options.config_path,
            cluster_config: Arc::new(RwLock::new(options.cluster_config)),
            key_path: options.key_path,
            key,
            sessions: Arc::new(SessionStore::new()),
            oidc,
            pending_edit: Arc::new(Mutex::new(None)),
            started_at: Instant::now(),
            session_ttl,
            rollback_after: crate::config_edit::DEFAULT_ROLLBACK_AFTER,
            self_update_artifact_path: options.self_update_artifact_path,
            shutdown: options.shutdown,
            http: options.http,
            tls_fingerprint,
        };

        // First launch of a fresh cluster: mint an api key, since it is the
        // only way in until either OIDC is configured or a key already exists.
        // Backgrounded because at this point the cluster usually has no leader
        // yet and the write would have nowhere to go.
        routes::auth_keys::spawn_bootstrap_first_key(state.clone());

        // Deliver the cluster key to nodes that lack it, and receive it here
        // if this node is the one lacking it, both backgrounded for the
        // same reason: no leader yet on a fresh cluster, or simply nothing to
        // do until another node publishes or wraps a key.
        routes::key_distribution::spawn_wrap_for_new_nodes(state.clone());
        routes::key_distribution::spawn_unwrap_own_key(state.clone(), state.node_name.clone(), node_keypair);

        Ok(Self {
            router: build_router(state),
            listen_addr: options.listen_addr,
            plain_listen_addr: options.plain_listen_addr,
            tls_cert_pem: options.tls_cert_pem,
            tls_key_pem: options.tls_key_pem,
        })
    }

    /// Serve until one of the listeners stops.
    ///
    /// The TLS listener is the one an operator reaches directly and is never
    /// optional: it is the way back in when the proxy cannot bind its ports.
    /// The plain one exists only for the reverse proxy, which speaks HTTP to
    /// its backends, and is bound to loopback so nothing off this machine can
    /// reach the dashboard without TLS.
    pub async fn serve(self) -> Result<(), ApiError> {
        let config = axum_server::tls_rustls::RustlsConfig::from_pem(self.tls_cert_pem, self.tls_key_pem)
            .await
            .map_err(|e| ApiError::Internal(format!("loading tls config: {e}")))?;

        let tls = axum_server::bind_rustls(self.listen_addr, config)
            .serve(self.router.clone().into_make_service());

        let Some(plain_addr) = self.plain_listen_addr else {
            return tls
                .await
                .map_err(|e| ApiError::Internal(format!("serving: {e}")));
        };

        let plain = axum_server::bind(plain_addr).serve(self.router.into_make_service());
        tokio::select! {
            result = tls => result.map_err(|e| ApiError::Internal(format!("serving tls: {e}"))),
            result = plain => result.map_err(|e| ApiError::Internal(format!("serving plain http: {e}"))),
        }
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
        .fallback(get(serve_ui))
        .layer(CompressionLayer::new())
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Only the leader serves the dashboard: a follower's replicated state can lag
/// behind, and a client that loaded the page from one node then had requests
/// bounce between nodes as leadership moved was a worse experience than one
/// redirect up front. A follower instead sends the browser straight to the
/// leader's address, where every subsequent asset and api call lands on the
/// same node the page was actually served from.
async fn serve_ui(State(state): State<AppState>, request: Request) -> Response {
    if state.cluster.is_leader() {
        return ui::serve(request).await;
    }
    match leader_location(&state).await {
        Some(leader) => {
            let target = format!("{}{}", leader.trim_end_matches('/'), request.uri().path());
            Redirect::temporary(&target).into_response()
        }
        None => (StatusCode::SERVICE_UNAVAILABLE, "no cluster leader elected yet").into_response(),
    }
}

/// Only the leader has a fully caught up view of the cluster and can accept
/// writes, so every `/api` and `/auth` route bounces elsewhere when this node
/// is not it.
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

#[cfg(test)]
mod fingerprint_tests {
    use super::leaf_fingerprint;

    #[test]
    fn hashes_the_decoded_leaf_certificate() {
        use base64::Engine;
        use sha2::{Digest, Sha256};

        let der = b"not a real certificate, just some bytes to hash";
        let encoded = base64::engine::general_purpose::STANDARD.encode(der);
        let pem = format!("-----BEGIN CERTIFICATE-----\n{encoded}\n-----END CERTIFICATE-----\n");

        let expected = Sha256::digest(der)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(leaf_fingerprint(pem.as_bytes()).unwrap(), expected);
    }

    #[test]
    fn only_hashes_the_first_certificate_in_a_chain() {
        let der_a = b"leaf";
        let der_b = b"intermediate, should be ignored";
        use base64::Engine;
        let a = base64::engine::general_purpose::STANDARD.encode(der_a);
        let b = base64::engine::general_purpose::STANDARD.encode(der_b);
        let pem = format!(
            "-----BEGIN CERTIFICATE-----\n{a}\n-----END CERTIFICATE-----\n\
             -----BEGIN CERTIFICATE-----\n{b}\n-----END CERTIFICATE-----\n"
        );

        use sha2::{Digest, Sha256};
        let expected = Sha256::digest(der_a)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(leaf_fingerprint(pem.as_bytes()).unwrap(), expected);
    }

    #[test]
    fn errors_without_a_certificate_marker() {
        assert!(leaf_fingerprint(b"not a pem at all").is_err());
    }
}
