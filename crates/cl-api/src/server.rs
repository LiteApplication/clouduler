//! Builds the axum `Router` and serves it over HTTPS.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
use tokio::sync::{Mutex, RwLock};
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

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
    /// Instances this node's reconciler is currently pulling the image for.
    /// Read by `routes::instances` to report a "pulling" state that a docker
    /// inspect alone cannot distinguish from "about to be created".
    pub pulling: cl_sched::PullTracker,
    /// Live per-volume progress for an instance's in-progress migration
    /// sync. See `cl_drive::MigrationSyncTracker`.
    pub migration_sync: cl_drive::MigrationSyncTracker,
    /// Name of the node this api instance is running on. Used for the local
    /// half of node detail (resource usage, daemon logs) that has no RPC path
    /// to another node's process.
    pub node_name: String,
    pub config_path: PathBuf,
    pub cluster_config: Arc<RwLock<ClusterConfig>>,
    pub key_path: PathBuf,
    pub key: Arc<RwLock<Option<Arc<ClusterKey>>>>,
    /// Broadcasts every key this node comes to hold — automatic delivery
    /// (`key_distribution`) or a manual upload (`routes::objects::post_key`)
    /// — to the rest of the daemon (materializer, proxy, self-update), which
    /// hold `key_tx`'s `Receiver` rather than a snapshot loaded once at boot.
    pub key_tx: tokio::sync::watch::Sender<Option<Arc<ClusterKey>>>,
    pub sessions: Arc<SessionStore>,
    /// Rebuilt live by `spawn_sync_cluster_config` whenever the replicated
    /// `auth.oidc` section changes, not just resolved once at boot — so
    /// turning OIDC on, off, or pointing it at a different issuer takes
    /// effect on every node without a restart, not just the one that
    /// accepted the edit.
    pub oidc: Arc<RwLock<Option<Arc<OidcAuthenticator>>>>,
    pub pending_edit: Arc<Mutex<Option<PendingEdit>>>,
    /// Fixed point `config_edit`'s pure clock counts from.
    pub started_at: Instant,
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
    /// This node's own reverse proxy's latest active health check per route,
    /// keyed `stack.service`. Read by `routes::instances` to report a backend
    /// the proxy can't reach without waiting for a real client to hit it.
    pub proxy_health: cl_proxy::HealthTable,
    /// This node's data directory. Only `routes::git_backup`'s webhook needs
    /// it, for a second git working copy (separate from the leader export
    /// loop's own, under `daemon::watch_git_backup`) to fetch and check out a
    /// candidate commit into before validating it.
    pub data_dir: PathBuf,
    /// Internal-route signatures already accepted; see
    /// `auth::internal::ReplayCache`.
    pub internal_replay: Arc<crate::auth::internal::ReplayCache>,
    /// Pending oauth authorization requests and codes, leader-local; see
    /// `auth::oauth`.
    pub oauth: Arc<crate::auth::oauth::OAuthMemory>,
    /// The `/mcp` protocol service and its sessions; see `mcp`.
    pub mcp: Arc<crate::mcp::McpHub>,
    /// Wakes the daemon's dns, certificate and git export loops early.
    pub nudges: crate::Nudges,
    /// This node's live routing table, for `/api/proxy/routes`.
    pub proxy_routes: cl_proxy::SharedRoutes,
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
    pub pulling: cl_sched::PullTracker,
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
    /// Plain HTTP listener the reverse proxy forwards dashboard traffic to.
    /// `None` leaves it off. Always a loopback address; see `WebConfig`.
    pub plain_listen_addr: Option<SocketAddr>,
    pub self_update_artifact_path: PathBuf,
    pub shutdown: tokio::sync::watch::Sender<()>,
    pub http: reqwest::Client,
    /// This node's own asymmetric keypair, used only to unwrap a cluster key
    /// delivered automatically through raft. See `routes::key_distribution`.
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
    /// Needed only to poll for a freshly issued certificate for `fqdn`
    /// after boot; unrelated to anything a request handler touches.
    cluster: Arc<ClusterNode>,
    fqdn: String,
    /// A clone of the same `AppState` every handler runs against, handed to
    /// `cl-daemon`'s `watch_git_backup` so its export loop can pull through
    /// the exact same reconcile path a webhook uses (see `routes::git_backup`)
    /// before it clobbers a pending git-side change with a raft-driven export.
    state: AppState,
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
            migration_sync: options.migration_sync,
            node_name: options.node_name,
            config_path: options.config_path,
            cluster_config: Arc::new(RwLock::new(options.cluster_config)),
            key_path: options.key_path,
            key,
            key_tx: options.key_tx,
            sessions: Arc::new(SessionStore::new(cluster.clone())),
            oidc: Arc::new(RwLock::new(oidc)),
            pending_edit: Arc::new(Mutex::new(None)),
            started_at: Instant::now(),
            rollback_after: crate::config_edit::DEFAULT_ROLLBACK_AFTER,
            self_update_artifact_path: options.self_update_artifact_path,
            shutdown: options.shutdown,
            http: options.http,
            tls_fingerprint,
            proxy_health: options.proxy_health,
            data_dir: options.data_dir,
            nudges: options.nudges,
            proxy_routes: options.proxy_routes,
            internal_replay: Default::default(),
            oauth: Default::default(),
            mcp: Default::default(),
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
        routes::key_distribution::spawn_unwrap_own_key(
            state.clone(),
            state.node_name.clone(),
            node_keypair,
        );

        // Keeps `state.cluster_config` (and, when it changes, `state.oidc`)
        // in step with the replicated config on every node, not just the one
        // whose own `PUT /api/config` handler ran. Without this, a node that
        // only ever learns of a config edit through raft replication would
        // serve a stale `session_ttl`/OIDC setup and a stale leader address
        // forever.
        spawn_sync_cluster_config(state.clone());

        // Neither store evicts on its own: sessions only when presented
        // again, pending logins only when the cap is hit.
        spawn_prune_expired(state.clone());

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

    /// The same `AppState` every handler runs against - see the field doc
    /// comment for why `cl-daemon` needs this.
    pub fn state(&self) -> AppState {
        self.state.clone()
    }
}

/// How often expired sessions and unfinished OIDC logins are swept.
const PRUNE_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Periodically drops expired sessions and abandoned OIDC logins, until the
/// daemon shuts down.
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

/// Build the OIDC authenticator this config's `auth.oidc` section describes,
/// or `None` if it has none. Used both at boot and by
/// `spawn_sync_cluster_config` when a live config edit changes the section,
/// so login stays in step with the replicated config on every node, not just
/// the one that accepted the edit.
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

/// How often the replicated cluster config is polled to keep `state.cluster_config`
/// live. Same order of magnitude as the daemon's own `watch_cluster_config`.
const CONFIG_SYNC_INTERVAL: Duration = Duration::from_secs(5);

/// Keep `state.cluster_config` matching the replicated config on every node,
/// not just the one whose own `PUT /api/config` handler already updates it
/// in-process for immediate feedback (`routes::config::apply_config`). Also
/// rebuilds `state.oidc` whenever the `auth.oidc` section actually changes,
/// so enabling, disabling or repointing OIDC login takes effect everywhere
/// without a restart.
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

/// How often to check replicated state for a certificate change. Same order
/// of magnitude as `crate::proxy::RECONCILE_INTERVAL`, which is the fastest
/// this could possibly change.
const CERT_RELOAD_INTERVAL: Duration = Duration::from_secs(5);

/// Poll replicated state for `fqdn`'s certificate and, whenever the stored
/// bytes differ from what's currently served, reload them into the already
/// bound TLS listener — the same `RustlsConfig::reload_from_pem` mechanism
/// `cl_proxy::CertStore` already uses for every other hostname the reverse
/// proxy terminates, applied here for the web ui's own listener for the
/// first time. Without this, a self-signed certificate issued at boot (or
/// one nearing expiry) never gets replaced until the daemon restarts, even
/// after ACME issues a real one.
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
    /// Serve until one of the listeners stops.
    ///
    /// The TLS listener is the one an operator reaches directly and is never
    /// optional: it is the way back in when the proxy cannot bind its ports.
    /// The plain one exists only for the reverse proxy, which speaks HTTP to
    /// its backends, and is bound to loopback so nothing off this machine can
    /// reach the dashboard without TLS.
    pub async fn serve(self) -> Result<(), ApiError> {
        let config =
            axum_server::tls_rustls::RustlsConfig::from_pem(self.tls_cert_pem, self.tls_key_pem)
                .await
                .map_err(|e| ApiError::Internal(format!("loading tls config: {e}")))?;

        // The self signed cert loaded above is what a brand new cluster
        // starts with; once ACME issues a real one for this hostname
        // (`crate::proxy`'s `run_acme`, driven from `clouduler`), reload it
        // into the already-bound listener instead of leaving the stale one
        // in place until the next restart.
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

/// A router with no TLS wrapper, driven with `tower::ServiceExt::oneshot` in
/// tests. Public so `src/tests.rs` can build one from a hand-assembled
/// `AppState` without going through `ApiServer::new`'s OIDC discovery and
/// bootstrap key submission.
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

/// Only the leader serves the dashboard: a follower's replicated state can lag
/// behind, and a client that loaded the page from one node then had requests
/// bounce between nodes as leadership moved was a worse experience than one
/// hop up front. A follower relays the request to the leader rather than
/// redirecting to its bare IP address, since the dashboard's hostname may
/// still resolve to this node while DNS catches up with a leader change.
async fn serve_ui(State(state): State<AppState>, request: Request) -> Response {
    if state.cluster.is_leader() {
        return ui::serve(request).await;
    }
    relay_to_leader(&state, request).await
}

/// Largest request body a follower will buffer to relay to the leader.
const MAX_RELAYED_BODY: usize = 32 * 1024 * 1024;

/// Proxy a request to the leader unchanged (method, path, query, headers and
/// body) and stream its answer back. The response is not buffered, so event
/// streams and large assets pass through.
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
        // The leader is addressed by ip, and this node's own compression layer
        // handles encoding for the browser.
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

/// Only the leader has a fully caught up view of the cluster and can accept
/// writes, so every `/api` and `/auth` write is relayed to it when this node
/// is not it.
async fn redirect_non_leader(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Result<Response, ApiError> {
    if state.cluster.is_leader() {
        return Ok(next.run(request).await);
    }

    // The oauth flow keeps its pending requests and codes in the leader's
    // memory, so unlike other reads, authorize has to land there. A browser
    // follows the redirect; token and registration are relayed, since an MCP
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

    // MCP sessions and focus live in the serving node's memory, so every
    // method of `/mcp` (POST calls, the GET event stream, DELETE) is relayed
    // to the leader; a client will not follow a redirect.
    if path == "/mcp" {
        return Ok(crate::mcp::forward_to_leader(&state, request).await);
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

    // A git host's webhook config points at a fixed url that has to keep
    // working across leader elections, and cannot be expected to follow a
    // 503 + Location the way the ui's own fetch client does. Both routes
    // check their own auth (a shared token / an internal HMAC signature, not
    // `Identity`) and forward the actual work to the leader themselves - see
    // `routes::git_backup`.
    if path == "/api/git-backup/webhook" || path == "/api/git-backup/webhook/internal-trigger" {
        return Ok(next.run(request).await);
    }

    // Node-to-node routes (`/api/.../internal/...`), HMAC-signed by
    // `InternalAuth` rather than a session or api key: these carry out an
    // action that only makes sense on the specific node addressed - restart
    // itself, pull its own image, update its own container - not on
    // whichever node happens to be leader right now. Gating them here meant
    // every such call to a non-leader node (i.e. almost always, since most
    // nodes are workers) answered 503 "not the leader" instead of doing the
    // work, which is what made `/api/instances/internal/{id}/update` fail
    // for every instance not living on the leader.
    if path.contains("/internal/") {
        return Ok(next.run(request).await);
    }

    Ok(relay_to_leader(&state, request).await)
}

/// Best-effort address of the current leader, built from its internal rpc
/// address plus the configured web port. The cluster does not track a
/// separate api-facing address per node, so this is the closest thing to it.
///
/// Reads the address from raft's own membership (via `leader_rpc_addr`)
/// rather than the replicated application-level node roster: the roster can
/// lag behind on a follower, and briefly disagree entirely about a leader's
/// id after it gets re-provisioned, which could make a perfectly healthy
/// leader lookup fail.
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
