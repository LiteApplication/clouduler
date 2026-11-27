//! Builds the axum `Router` and serves it over HTTPS.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use cl_config::ClusterConfig;
use cl_docker::Docker;
use cl_raft::ClusterNode;
use cl_secret::ClusterKey;
use tokio::sync::RwLock;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use crate::auth::oidc::OidcAuthenticator;
use crate::auth::session::SessionStore;
use crate::error::ApiError;
use crate::routes;
use crate::ui;

/// Everything a handler needs; cheap to clone (all shared state).
#[derive(Clone)]
pub struct AppState {
    pub cluster: Arc<ClusterNode>,
    /// Empty when this node has no docker daemon. The api still serves everything
    /// else: a node whose docker is down still votes and must stay reachable to be
    /// fixed. Shared because the daemon waits for dockerd on its own schedule.
    pub docker: cl_docker::DockerHandle,
    /// Instances whose image this node is pulling; lets `routes::instances` report
    /// "pulling" instead of "about to be created".
    pub pulling: cl_sched::PullTracker,
    /// This node's rolling probe history of its peers, filled by `routes::links::spawn_link_probes`.
    pub link_health: cl_sched::LinkHealth,
    pub migration_sync: cl_drive::MigrationSyncTracker,
    pub node_name: String,
    pub config_path: PathBuf,
    pub cluster_config: Arc<RwLock<ClusterConfig>>,
    pub key_path: PathBuf,
    pub key: Arc<RwLock<Option<Arc<ClusterKey>>>>,
    /// Every key this node comes to hold (automatic delivery or manual upload),
    /// broadcast to the materializer, proxy and self-update.
    pub key_tx: tokio::sync::watch::Sender<Option<Arc<ClusterKey>>>,
    pub sessions: Arc<SessionStore>,
    /// Rebuilt live by `spawn_sync_cluster_config` when `auth.oidc` changes, so OIDC
    /// edits take effect on every node without a restart.
    pub oidc: Arc<RwLock<Option<Arc<OidcAuthenticator>>>>,
    pub self_update_artifact_path: PathBuf,
    /// Fires the daemon's clean shutdown path (as `SIGINT` does) after a self-update.
    pub shutdown: tokio::sync::watch::Sender<()>,
    /// Forwards reads to the node that holds the answer. Accepts invalid certs:
    /// peers are authenticated by the HMAC header, not TLS trust (self-signed until ACME).
    pub http: reqwest::Client,
    /// SHA-256 fingerprint (colon-hex, as `clouduler init --tls-fingerprint`) of the
    /// certificate actually served, computed from the bytes given to the listener.
    pub tls_fingerprint: String,
    /// This node's proxy health checks per route, keyed `stack.service`.
    pub proxy_health: cl_proxy::HealthTable,
    /// Only the git-backup webhook needs it: a second working copy, separate from
    /// the leader export loop's, to check out a candidate commit before validating.
    pub data_dir: PathBuf,
    pub internal_replay: Arc<crate::auth::internal::ReplayCache>,
    pub oauth: Arc<crate::auth::oauth::OAuthMemory>,
    pub mcp: Arc<crate::mcp::McpHub>,
    pub nudges: crate::Nudges,
    pub git_export_author: Arc<parking_lot::Mutex<Option<(String, String)>>>,
    pub proxy_routes: cl_proxy::SharedRoutes,
}

impl AppState {
    pub async fn docker(&self) -> Result<Docker, ApiError> {
        self.docker.get().await.ok_or_else(|| {
            ApiError::Unavailable(
                "this node has no docker daemon, so it cannot answer for containers".to_string(),
            )
        })
    }
}

pub struct ApiServerOptions {
    pub cluster: Arc<ClusterNode>,
    pub docker: cl_docker::DockerHandle,
    pub pulling: cl_sched::PullTracker,
    pub link_health: cl_sched::LinkHealth,
    pub migration_sync: cl_drive::MigrationSyncTracker,
    pub node_name: String,
    pub config_path: PathBuf,
    pub cluster_config: ClusterConfig,
    pub key_path: PathBuf,
    pub key: Option<Arc<ClusterKey>>,
    pub key_tx: tokio::sync::watch::Sender<Option<Arc<ClusterKey>>>,
    pub tls_cert_pem: Vec<u8>,
    pub tls_key_pem: Vec<u8>,
    pub listen_addr: SocketAddr,
    /// Plain HTTP listener for the reverse proxy; `None` leaves it off. Loopback only.
    pub plain_listen_addr: Option<SocketAddr>,
    pub self_update_artifact_path: PathBuf,
    pub shutdown: tokio::sync::watch::Sender<()>,
    pub http: reqwest::Client,
    pub node_keypair: Option<cl_secret::NodeKeypair>,
    pub proxy_health: cl_proxy::HealthTable,
    pub data_dir: PathBuf,
    pub nudges: crate::Nudges,
    pub proxy_routes: cl_proxy::SharedRoutes,
}

pub struct ApiServer {
    router: Router,
    listen_addr: SocketAddr,
    plain_listen_addr: Option<SocketAddr>,
    tls_cert_pem: Vec<u8>,
    tls_key_pem: Vec<u8>,
    cluster: Arc<ClusterNode>,
    fqdn: String,
    /// Handed to `cl-daemon`'s `watch_git_backup` so its export loop pulls through
    /// the same reconcile path as a webhook before clobbering a pending git-side change.
    state: AppState,
}

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
    Ok(digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":"))
}

impl ApiServer {
    pub async fn new(options: ApiServerOptions) -> Result<Self, ApiError> {
        let tls_fingerprint = leaf_fingerprint(&options.tls_cert_pem)?;
        let fqdn = options.cluster_config.web_fqdn();
        let key = Arc::new(RwLock::new(options.key));
        let oidc = build_oidc(&options.cluster_config, &options.cluster, &key)?;
        let cluster = options.cluster.clone();

        let node_keypair = options.node_keypair;
        let state = AppState {
            cluster: options.cluster,
            docker: options.docker,
            pulling: options.pulling,
            link_health: options.link_health,
            migration_sync: options.migration_sync,
            node_name: options.node_name,
            config_path: options.config_path,
            cluster_config: Arc::new(RwLock::new(options.cluster_config)),
            key_path: options.key_path,
            key,
            key_tx: options.key_tx,
            sessions: Arc::new(SessionStore::new(cluster.clone())),
            oidc: Arc::new(RwLock::new(oidc)),
            self_update_artifact_path: options.self_update_artifact_path,
            shutdown: options.shutdown,
            http: options.http,
            tls_fingerprint,
            proxy_health: options.proxy_health,
            data_dir: options.data_dir,
            nudges: options.nudges,
            git_export_author: Default::default(),
            proxy_routes: options.proxy_routes,
            internal_replay: Default::default(),
            oauth: Default::default(),
            mcp: Default::default(),
        };

        // Mint the first api key: the only way in until OIDC or a key exists. Backgrounded
        // because a fresh cluster has no leader yet.
        routes::auth_keys::spawn_bootstrap_first_key(state.clone());

        // Backgrounded for the same reason: no leader yet on a fresh cluster.
        routes::key_distribution::spawn_wrap_for_new_nodes(state.clone());
        routes::key_distribution::spawn_unwrap_own_key(
            state.clone(),
            state.node_name.clone(),
            node_keypair,
        );

        // Without this, a node that only learns of a config edit through raft would
        // serve a stale `session_ttl`/OIDC setup and leader address forever.
        spawn_sync_cluster_config(state.clone());

        // Neither store evicts on its own.
        spawn_prune_expired(state.clone());

        crate::routes::links::spawn_link_probes(state.clone());

        crate::alerts::spawn(state.clone());
        crate::alerts::producers::spawn(state.clone());

        crate::auto_update::spawn(state.clone());

        Ok(Self {
            router: build_router(state.clone()),
            listen_addr: options.listen_addr,
            plain_listen_addr: options.plain_listen_addr,
            tls_cert_pem: options.tls_cert_pem,
            tls_key_pem: options.tls_key_pem,
            cluster,
            fqdn,
            state,
        })
    }

    pub fn state(&self) -> AppState {
        self.state.clone()
    }
}

const PRUNE_INTERVAL: Duration = Duration::from_secs(5 * 60);

fn spawn_prune_expired(state: AppState) {
    let mut shutdown = state.shutdown.subscribe();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PRUNE_INTERVAL);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => return,
            }
            state.sessions.prune_expired().await;
            state.oauth.prune_expired();
            if let Some(oidc) = state.oidc.read().await.as_ref() {
                oidc.prune_expired().await;
            }
        }
    });
}

fn build_oidc(
    cfg: &ClusterConfig,
    cluster: &Arc<ClusterNode>,
    key: &Arc<RwLock<Option<Arc<ClusterKey>>>>,
) -> Result<Option<Arc<OidcAuthenticator>>, ApiError> {
    let Some(oidc_cfg) = &cfg.auth.oidc else {
        return Ok(None);
    };
    let redirect_url = format!("https://{}/auth/oidc/callback", cfg.web_fqdn());
    Ok(Some(Arc::new(OidcAuthenticator::new(
        crate::auth::oidc::OidcSettings {
            issuer: oidc_cfg.issuer.clone(),
            client_id: oidc_cfg.client_id.clone(),
            redirect_url,
            scopes: oidc_cfg.scopes.clone(),
            admin_claim: oidc_cfg.admin_claim.clone(),
            admin_value: oidc_cfg.admin_value.clone(),
            username_claim: oidc_cfg.username_claim.clone(),
        },
        cluster.clone(),
        key.clone(),
    )?)))
}

const CONFIG_SYNC_INTERVAL: Duration = Duration::from_secs(5);

/// Also rebuilds `state.oidc` when `auth.oidc` changes. `PUT /api/config` updates
/// the local state in-process; this covers every other node.
fn spawn_sync_cluster_config(state: AppState) {
    tokio::spawn(async move {
        let mut applied_generation = 0u64;
        let mut ticker = tokio::time::interval(CONFIG_SYNC_INTERVAL);
        loop {
            ticker.tick().await;
            let Some(entry) = state.cluster.state().await.cluster_config else {
                continue;
            };
            if entry.generation == applied_generation {
                continue;
            }
            let parsed = match ClusterConfig::parse(&entry.yaml) {
                Ok(cfg) => cfg,
                Err(e) => {
                    warn!(error = %e, "replicated cluster config failed to parse, not applying it here");
                    continue;
                }
            };
            applied_generation = entry.generation;

            let oidc_changed = state.cluster_config.read().await.auth.oidc != parsed.auth.oidc;
            *state.cluster_config.write().await = parsed.clone();

            if oidc_changed {
                match build_oidc(&parsed, &state.cluster, &state.key) {
                    Ok(rebuilt) => *state.oidc.write().await = rebuilt,
                    Err(e) => {
                        warn!(error = %e, "could not rebuild the oidc authenticator from the updated config")
                    }
                }
            }
        }
    });
}

const CERT_RELOAD_INTERVAL: Duration = Duration::from_secs(5);

/// Reload the served certificate into the bound TLS listener when the stored one
/// changes, so a self-signed boot cert is replaced once ACME issues a real one.
async fn reload_certificate_on_change(
    cluster: Arc<ClusterNode>,
    fqdn: String,
    config: axum_server::tls_rustls::RustlsConfig,
) {
    let mut served: Option<Vec<u8>> = None;
    let mut ticker = tokio::time::interval(CERT_RELOAD_INTERVAL);
    loop {
        ticker.tick().await;
        let Some(stored) = cluster.state().await.certificates.get(&fqdn).cloned() else {
            continue;
        };
        if served.as_deref() == Some(stored.chain_pem.as_bytes()) {
            continue;
        }
        match config
            .reload_from_pem(
                stored.chain_pem.clone().into_bytes(),
                stored.key_pem.clone().into_bytes(),
            )
            .await
        {
            Ok(()) => {
                info!(%fqdn, "reloaded the web ui's tls certificate");
                served = Some(stored.chain_pem.into_bytes());
            }
            Err(e) => warn!(%fqdn, error = %e, "could not reload the web ui's tls certificate"),
        }
    }
}

impl ApiServer {
    /// The TLS listener is never optional: it is the way back in when the proxy
    /// cannot bind. The plain one is loopback-only, for the reverse proxy.
    pub async fn serve(self) -> Result<(), ApiError> {
        let config =
            axum_server::tls_rustls::RustlsConfig::from_pem(self.tls_cert_pem, self.tls_key_pem)
                .await
                .map_err(|e| ApiError::Internal(format!("loading tls config: {e}")))?;

        tokio::spawn(reload_certificate_on_change(
            self.cluster.clone(),
            self.fqdn.clone(),
            config.clone(),
        ));

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

/// A router with no TLS wrapper, for `oneshot` in `src/tests.rs`.
#[doc(hidden)]
pub fn build_router(state: AppState) -> Router {
    let api = routes::router().route_layer(middleware::from_fn_with_state(
        state.clone(),
        redirect_non_leader,
    ));

    Router::new()
        .merge(api)
        .fallback(get(serve_ui))
        .layer(CompressionLayer::new())
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Only the leader serves the dashboard: follower state can lag, and bouncing
/// between nodes as leadership moves is worse than one hop. Relays rather than
/// redirecting to a bare IP, since the hostname may still resolve here during DNS catch-up.
async fn serve_ui(State(state): State<AppState>, request: Request) -> Response {
    if state.cluster.is_leader() {
        return ui::serve(&state.data_dir, request).await;
    }
    relay_to_leader(&state, request).await
}

const MAX_RELAYED_BODY: usize = 32 * 1024 * 1024;

async fn relay_to_leader(state: &AppState, request: Request) -> Response {
    let Some(leader) = leader_location(state).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no cluster leader elected yet",
        )
            .into_response();
    };
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_RELAYED_BODY).await else {
        return ApiError::BadRequest("request body too large".into()).into_response();
    };
    let pq = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| parts.uri.path());
    let url = format!("{}{}", leader.trim_end_matches('/'), pq);
    let mut req = state.http.request(parts.method.clone(), url).body(bytes);
    for (name, value) in &parts.headers {
        if matches!(
            *name,
            header::HOST
                | header::CONNECTION
                | header::TRANSFER_ENCODING
                | header::CONTENT_LENGTH
                | header::ACCEPT_ENCODING
        ) {
            continue;
        }
        req = req.header(name, value);
    }
    send_to_leader(req).await
}

/// Sends a prepared leader request and streams its answer back, minus hop-by-hop headers.
pub(crate) async fn send_to_leader(req: reqwest::RequestBuilder) -> Response {
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return ApiError::Unavailable(format!("could not reach the leader: {e}"))
                .into_response()
        }
    };
    let status = resp.status();
    let headers = resp.headers().clone();
    let mut out = Response::new(Body::from_stream(resp.bytes_stream()));
    *out.status_mut() = status;
    for (name, value) in &headers {
        if matches!(
            *name,
            header::TRANSFER_ENCODING | header::CONNECTION | header::CONTENT_LENGTH
        ) {
            continue;
        }
        out.headers_mut().append(name.clone(), value.clone());
    }
    out
}

async fn redirect_non_leader(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Result<Response, ApiError> {
    if state.cluster.is_leader() {
        return Ok(next.run(request).await);
    }

    // Oauth pending state is in the leader's memory, so authorize must land there. A
    // browser follows the redirect; token and registration are relayed since an MCP
    // client will not follow a 503.
    let path = request.uri().path();
    if path == "/oauth/authorize" {
        return Ok(match leader_location(&state).await {
            Some(leader) => {
                let pq = request
                    .uri()
                    .path_and_query()
                    .map(|pq| pq.as_str())
                    .unwrap_or(path);
                Redirect::temporary(&format!("{}{}", leader.trim_end_matches('/'), pq))
                    .into_response()
            }
            None => ApiError::NotLeader { leader: None }.into_response(),
        });
    }
    if request.method() == axum::http::Method::POST
        && (path == "/oauth/token" || path == "/oauth/register")
    {
        return Ok(routes::oauth::forward_to_leader(&state, request).await);
    }

    // MCP sessions live in the serving node's memory, so every `/mcp` method is
    // relayed to the leader; a client will not follow a redirect.
    if path == "/mcp" {
        return Ok(crate::mcp::forward_to_leader(&state, request).await);
    }

    // Writes go to the leader (ordering); reads answer anywhere, since live container
    // state, resource usage and daemon logs exist only on the node they describe.
    // A lagging follower may answer slightly stale; `/api/status` carries `state_version`.
    if request.method() == axum::http::Method::GET {
        return Ok(next.run(request).await);
    }

    // Login and callback POSTs must work everywhere, or a client stuck on a follower
    // can never log in to discover the leader.
    let path = request.uri().path();
    if path.starts_with("/auth/oidc/") {
        return Ok(next.run(request).await);
    }

    // A git host's webhook url is fixed and cannot follow a 503 + Location. Both routes
    // check their own auth and forward to the leader themselves (`routes::git_backup`).
    if path == "/api/git-backup/webhook" || path == "/api/git-backup/webhook/internal-trigger" {
        return Ok(next.run(request).await);
    }

    // Node-to-node routes, HMAC-signed by `InternalAuth`, act on the addressed node
    // (restart, pull, update itself). Gating them on leader made every call to a
    // worker answer 503 "not the leader".
    if path.contains("/internal/") {
        return Ok(next.run(request).await);
    }

    Ok(relay_to_leader(&state, request).await)
}

/// Best-effort leader address: its raft rpc address plus the web port. Read from
/// raft membership, not the replicated roster, which can lag or briefly disagree
/// about the leader's id after re-provisioning.
pub(crate) async fn leader_location(state: &AppState) -> Option<String> {
    let addr = state.cluster.leader_rpc_addr().await?;
    let ip = addr.rsplit_once(':').map(|(ip, _)| ip).unwrap_or(&addr);
    let port = state.cluster_config.read().await.web.port;
    Some(format!("https://{ip}:{port}/"))
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
