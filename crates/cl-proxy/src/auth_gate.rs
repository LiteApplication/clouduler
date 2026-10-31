//! `authenticated` middleware: gate a route behind a login, so an app with no
//! login of its own can still be protected, optionally for some groups only.
//!
//! # Shape
//!
//! A *portal* hostname per network (`proxy.auth.host` under each of
//! `proxy.auth.networks`' domains) runs the OIDC login, so a login is once per
//! network and a network that is not listed offers none. A protected app
//! never talks to the identity provider itself:
//!
//! 1. A request with no valid `cl_proxy_auth` cookie is redirected (302) to
//!    `https://<portal>/login?callback=<the url it asked for>`.
//! 2. If the browser already has a portal session, the portal answers with a
//!    302 straight back to `https://<app>/.well-known/clouduler-cookie?token=…
//!    &redirect=…`. Otherwise it shows a login page; after the OIDC round trip
//!    it does the same.
//! 3. The app's cookie endpoint spends the single-use token, which mints a
//!    session for that host, sets the cookie and redirects to the original
//!    page. Nothing in steps 1-3 after the first visit needs a click or any
//!    javascript: it is three redirects.
//!
//! # State
//!
//! Sessions and tokens live in raft (`ClusterState::proxy_sessions`,
//! `proxy_tokens`), keyed by a SHA-256 of the cookie value / token, so any
//! node accepts any login, a login survives restarts, and a token cannot be
//! spent twice even by two nodes at once (`Request::RedeemProxyToken` is
//! applied atomically). Only the in-flight OIDC state (csrf, nonce, pkce) is
//! kept in a short-lived sealed cookie instead, so the callback can land on a
//! different node and a forged callback carries no matching cookie.
//!
//! The cookie is host-only, so each protected hostname has its own session,
//! bound to that host in the stored record and to the portal session it came
//! from (`ProxySessionRecord::parent`): signing out at the portal removes the
//! portal session and with it every app session handed out from it. The proxy strips its own cookies
//! from requests before forwarding, so a backend never sees a session.
//!
//! This uses its own identity provider client (`proxy.auth`, secret
//! `_clouduler_proxy_oidc_client_secret`) and its own groups, separate from
//! the `auth.oidc` that logs operators into the dashboard.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use cl_config::{Authenticated, ClusterConfig, PortalPalette, ProxyAuthConfig};
use cl_raft::node::ClusterNode;
use cl_raft::{ProxySessionRecord, ProxyTokenRecord, Request as RaftRequest};
use cl_secret::{ClusterKey, Sealed};
use http::{HeaderValue, Uri};
use hyper::body::Incoming;
use hyper::header::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE, COOKIE, HOST, LOCATION, SET_COOKIE};
use hyper::HeaderMap;
use hyper::{Method, Request, Response, StatusCode};
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tracing::warn;

use crate::route::RouteTable;
use crate::server::{empty_body, text_body, ProxyBody};

/// Reserved name of the sealed cluster secret holding the proxy login's OIDC
/// client secret. Distinct from the dashboard's `_clouduler_oidc_client_secret`.
const OIDC_CLIENT_SECRET: &str = "_clouduler_proxy_oidc_client_secret";

/// Answered on every protected hostname: spends a token, sets the cookie.
pub const COOKIE_PATH: &str = "/.well-known/clouduler-cookie";
/// Answered on every protected hostname: signs out at the portal.
pub const LOGOUT_PATH: &str = "/.well-known/clouduler-logout";
/// Prefix of every path the gate answers itself on a protected hostname.
const RESERVED_PREFIX: &str = "/.well-known/clouduler-";
/// Cookie carrying a protected hostname's session id.
pub const COOKIE_NAME: &str = "cl_proxy_auth";
/// Cookie carrying the portal's own session id.
pub const PORTAL_COOKIE_NAME: &str = "cl_proxy_portal";
/// Cookie carrying the sealed in-flight OIDC state on the portal.
pub const LOGIN_COOKIE_NAME: &str = "cl_proxy_login";
const LOGIN_COOKIE_PATH: &str = "/callback";
const LOGIN_SEAL_CONTEXT: &str = "proxy-login";
const METADATA_TTL: Duration = Duration::from_secs(3600);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long a portal-to-app token may wait to be spent. It only has to
/// survive one redirect.
const TOKEN_TTL: Duration = Duration::from_secs(60);
const ID_LEN: usize = 32;

/// In-flight login state, sealed into the `cl_proxy_login` cookie.
#[derive(Serialize, Deserialize)]
struct PendingLogin {
    state: String,
    nonce: String,
    pkce_verifier: String,
    /// The protected url the user was after; empty for a direct portal visit.
    callback: String,
    started_at_unix: u64,
    /// The client address the visitor agreed to save, confirmed on the
    /// portal. Honoured at the callback only if it is still the peer.
    #[serde(default)]
    remember_ip: Option<String>,
}

/// What the login page offers when the visitor's address may be saved.
struct RememberOffer {
    ip: IpAddr,
    network: String,
    /// "30 days", for the checkbox and the confirmation page.
    lasts: String,
    /// The protected url the visitor was after, if any.
    callback: Option<String>,
}

/// Key of a saved address in `proxy_sessions`. Saved addresses ride the same
/// replicated map and expiry as sessions, under a key no cookie value can
/// reach: a lookup also requires `host` to equal [`trusted_host`], which no
/// real hostname does.
fn trusted_key(network: &str, ip: IpAddr) -> String {
    hash_id(&format!("trusted-ip|{network}|{}", ip.to_canonical()))
}

fn trusted_host(network: &str, ip: IpAddr) -> String {
    format!(
        "{}{network}:{}",
        cl_raft::state::REMEMBERED_IP_HOST_PREFIX,
        ip.to_canonical()
    )
}

fn human_duration(d: Duration) -> String {
    let days = d.as_secs() / 86_400;
    let hours = d.as_secs() / 3_600;
    match (days, hours) {
        (0, 0) => "less than an hour".to_string(),
        (0, 1) => "1 hour".to_string(),
        (0, h) => format!("{h} hours"),
        (1, _) => "1 day".to_string(),
        (d, _) => format!("{d} days"),
    }
}

/// Who the identity provider says logged in.
struct Identity {
    subject: String,
    username: String,
    groups: Vec<String>,
}

/// One network's login portal.
struct Portal {
    auth: ProxyAuthConfig,
    network: String,
    /// Portal hostname on that network.
    fqdn: String,
}

/// What a request should get in response to the `authenticated` middleware.
pub enum GateOutcome {
    /// The session is valid; proceed to the backend.
    Allow,
    /// Answer directly: a redirect to log in, a cookie hand-out, or a refusal.
    Respond(Response<ProxyBody>),
}

/// Runtime state for the login portal and the `authenticated` middleware.
/// Reads config and the cluster key live on each request, since both can
/// change independently of the proxy's listeners.
pub struct AuthGate {
    config_rx: tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
    key_rx: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
    cluster: Arc<ClusterNode>,
    http: openidconnect::reqwest::Client,
    /// Cached discovery, invalidated whenever the configured issuer changes
    /// or the entry is older than `METADATA_TTL`.
    metadata: RwLock<Option<(String, Instant, CoreProviderMetadata)>>,
}

/// Builds a client from discovery, a secret and a redirect. A macro because
/// `CoreClient`'s type-state parameters change with each setter and are not
/// worth spelling out for a helper's return type.
macro_rules! build_client {
    ($metadata:expr, $settings:expr, $secret:expr, $redirect:expr) => {
        CoreClient::from_provider_metadata(
            $metadata,
            ClientId::new($settings.client_id.clone()),
            $secret.clone().map(ClientSecret::new),
        )
        .set_redirect_uri($redirect.clone())
    };
}

impl AuthGate {
    pub fn new(
        config_rx: tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
        key_rx: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
        cluster: Arc<ClusterNode>,
    ) -> Self {
        let http = openidconnect::reqwest::ClientBuilder::new()
            .redirect(openidconnect::reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client with no proxy or tls quirks always builds");
        Self {
            config_rx,
            key_rx,
            cluster,
            http,
            metadata: RwLock::new(None),
        }
    }

    /// The portal serving `network`, if `proxy.auth` lists it.
    fn portal_for_network(&self, network: &str) -> Option<Portal> {
        let cfg = self.config_rx.borrow();
        let auth = cfg.proxy.auth.as_ref().filter(|a| a.covers(network))?;
        Some(Portal {
            fqdn: auth.fqdn(&cfg, network),
            network: network.to_string(),
            auth: auth.clone(),
        })
    }

    /// The portal whose hostname is `host`.
    fn portal_for_host(&self, host: &str) -> Option<Portal> {
        let host = normalize_host(host);
        let cfg = self.config_rx.borrow();
        let auth = cfg.proxy.auth.as_ref()?;
        auth.portals(&cfg)
            .into_iter()
            .find(|(_, fqdn)| *fqdn == host)
            .map(|(network, fqdn)| Portal {
                auth: auth.clone(),
                network,
                fqdn,
            })
    }

    fn key(&self) -> Option<Arc<ClusterKey>> {
        self.key_rx.borrow().clone()
    }

    /// Whether `ip` may be saved on `network` right now: the network offers
    /// it and the address is one of that network's own clients.
    fn can_remember(&self, network: &str, ip: IpAddr) -> bool {
        let cfg = self.config_rx.borrow();
        cfg.proxy
            .auth
            .as_ref()
            .is_some_and(|a| a.remembers_ip(network))
            && cfg.ip_in_network(network, ip)
    }

    fn remember_offer(
        &self,
        settings: &ProxyAuthConfig,
        network: &str,
        peer: IpAddr,
        callback: Option<&str>,
    ) -> Option<RememberOffer> {
        self.can_remember(network, peer).then(|| RememberOffer {
            ip: peer.to_canonical(),
            network: network.to_string(),
            lasts: human_duration(settings.remember_ip_ttl),
            callback: callback.map(str::to_string),
        })
    }

    /// The saved login for `ip` on `network`, if it is still valid and the
    /// network still offers saving addresses.
    async fn trusted_ip(&self, network: &str, ip: IpAddr) -> Option<ProxySessionRecord> {
        if !self.can_remember(network, ip) {
            return None;
        }
        let key = trusted_key(network, ip);
        let host = trusted_host(network, ip);
        let now = now_unix() as i64;
        self.cluster
            .read(|state| {
                let r = state.proxy_sessions.get(&key)?;
                (r.expires_at > now && r.host == host).then(|| r.clone())
            })
            .await
    }

    /// Whether `host` (a `Host` header value) is the login portal.
    pub fn is_portal_host(&self, host: &str) -> bool {
        self.portal_for_host(host).is_some()
    }

    async fn client_secret(&self) -> Option<String> {
        let key = self.key()?;
        let sealed = self
            .cluster
            .read(|state| {
                state
                    .secrets
                    .get(OIDC_CLIENT_SECRET)
                    .map(|e| e.sealed.clone())
            })
            .await?;
        match Sealed::from_blob(sealed).open(&key, OIDC_CLIENT_SECRET) {
            Ok(plaintext) => String::from_utf8(plaintext).ok(),
            Err(e) => {
                warn!(error = %e, "cannot open the proxy login's oidc client secret");
                None
            }
        }
    }

    async fn metadata(&self, issuer: &str) -> Result<CoreProviderMetadata, String> {
        if let Some((cached_issuer, fetched_at, cached)) = self.metadata.read().await.as_ref() {
            if cached_issuer == issuer && fetched_at.elapsed() < METADATA_TTL {
                return Ok(cached.clone());
            }
        }
        self.refresh_metadata(issuer).await
    }

    async fn refresh_metadata(&self, issuer: &str) -> Result<CoreProviderMetadata, String> {
        let issuer_url =
            IssuerUrl::new(issuer.to_string()).map_err(|e| format!("bad issuer url: {e}"))?;
        let discovered = CoreProviderMetadata::discover_async(issuer_url, &self.http)
            .await
            .map_err(|e| format!("oidc discovery failed: {e}"))?;
        *self.metadata.write().await =
            Some((issuer.to_string(), Instant::now(), discovered.clone()));
        Ok(discovered)
    }

    // ---- sessions and tokens (raft) ----

    /// The live session for the cookie `name` on this request, if it was
    /// issued for `host`.
    async fn session(
        &self,
        headers: &HeaderMap,
        name: &str,
        host: &str,
    ) -> Option<(String, ProxySessionRecord)> {
        let id = extract_cookie(headers, name)?;
        if id.len() != ID_LEN {
            return None;
        }
        let key = hash_id(&id);
        let now = now_unix() as i64;
        let host = normalize_host(host);
        self.cluster
            .read(|state| {
                let s = state.proxy_sessions.get(&key)?;
                (s.expires_at > now && s.host == host).then(|| (key.clone(), s.clone()))
            })
            .await
    }

    /// Stores a session and returns the cookie value naming it.
    async fn create_session(&self, record: ProxySessionRecord) -> Result<String, String> {
        let id = random_id();
        let request = RaftRequest::PutProxySession {
            key: hash_id(&id),
            session: record,
            now: now_unix() as i64,
        };
        self.submit(request).await.map(|()| id)
    }

    async fn submit(&self, request: RaftRequest) -> Result<(), String> {
        let response = self
            .cluster
            .submit(request)
            .await
            .map_err(|e| e.to_string())?;
        match response.error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    // ---- the middleware ----

    /// Decide what a request to an `authenticated` route should get.
    /// `internal` says the TCP peer is a container or node of this cluster.
    pub async fn evaluate(
        &self,
        req: &Request<Incoming>,
        scheme: &str,
        policy: &Authenticated,
        network: Option<&str>,
        internal: bool,
        peer: IpAddr,
    ) -> GateOutcome {
        let Some(portal) = network.and_then(|n| self.portal_for_network(n)) else {
            return GateOutcome::Respond(status_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "this route requires login, but proxy.auth does not offer it on this network",
            ));
        };
        let Portal {
            auth: settings,
            fqdn: portal,
            network,
        } = portal;
        let Some(host) = req.headers().get(HOST).and_then(|v| v.to_str().ok()) else {
            return GateOutcome::Respond(status_response(
                StatusCode::BAD_REQUEST,
                "missing host header",
            ));
        };
        let host = host.to_string();

        if req.uri().path() == COOKIE_PATH {
            return GateOutcome::Respond(self.redeem(req, scheme, &host, &portal, &settings).await);
        }
        if req.uri().path() == LOGOUT_PATH {
            return GateOutcome::Respond(redirect_response(&format!(
                "https://{portal}/logout?callback={}",
                urlencode(&format!("{scheme}://{host}/"))
            )));
        }
        if policy.allow_internal && internal {
            return GateOutcome::Allow;
        }
        if let Some((_, session)) = self.session(req.headers(), COOKIE_NAME, &host).await {
            if permitted(policy, &session.groups) {
                return GateOutcome::Allow;
            }
            return GateOutcome::Respond(forbidden_page(
                &settings,
                &session.username,
                &portal,
                &format!("{scheme}://{host}{}", return_target(req.uri())),
            ));
        }
        if let Some(saved) = self.trusted_ip(&network, peer).await {
            if permitted(policy, &saved.groups) {
                return GateOutcome::Allow;
            }
        }
        if !wants_redirect(req) {
            return GateOutcome::Respond(status_response(
                StatusCode::UNAUTHORIZED,
                "login required",
            ));
        }
        let target = format!("{scheme}://{host}{}", return_target(req.uri()));
        GateOutcome::Respond(redirect_response(&format!(
            "https://{portal}/login?callback={}",
            urlencode(&target)
        )))
    }

    /// `/.well-known/clouduler-cookie?token=…&redirect=…`: spend the token,
    /// set this host's cookie, go to the page the user wanted.
    async fn redeem(
        &self,
        req: &Request<Incoming>,
        scheme: &str,
        host: &str,
        portal: &str,
        settings: &ProxyAuthConfig,
    ) -> Response<ProxyBody> {
        let params = form_urlencoded_parse(req.uri().query().unwrap_or(""));
        let target = match params.get("redirect") {
            Some(r) if is_safe_return(r) => r.clone(),
            _ => "/".to_string(),
        };
        let retry = format!(
            "https://{portal}/login?callback={}",
            urlencode(&format!("{scheme}://{host}{target}"))
        );
        let Some(token) = params.get("token").filter(|t| t.len() == ID_LEN) else {
            return message_page(
                StatusCode::BAD_REQUEST,
                settings,
                "Sign-in link incomplete",
                "This address is missing its sign-in token.",
                Some(("Sign in again", &retry)),
            );
        };

        let id = random_id();
        let session_key = hash_id(&id);
        let now = now_unix() as i64;
        let spent = self
            .submit(RaftRequest::RedeemProxyToken {
                token_key: hash_id(token),
                host: normalize_host(host),
                session_key: session_key.clone(),
                now,
            })
            .await;
        if let Err(e) = spent {
            warn!(error = %e, host, "proxy login token refused");
            return message_page(
                StatusCode::FORBIDDEN,
                settings,
                "Sign-in link expired",
                "This link was already used or has expired.",
                Some(("Sign in again", &retry)),
            );
        }
        let Some(expires_at) = self
            .cluster
            .read(|s| s.proxy_sessions.get(&session_key).map(|r| r.expires_at))
            .await
        else {
            return status_response(StatusCode::BAD_GATEWAY, "session was not stored");
        };

        let mut resp = redirect_response(&target);
        append_cookie(
            &mut resp,
            COOKIE_NAME,
            &id,
            "/",
            Some((expires_at - now).max(1) as u64),
            scheme == "https",
        );
        // The token is in this url; do not leak it onward.
        resp.headers_mut()
            .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
        resp
    }

    // ---- the portal ----

    /// Answers a request for the portal hostname itself.
    pub async fn handle_portal(
        &self,
        req: &Request<Incoming>,
        routes: &RouteTable,
        scheme: &str,
        peer: IpAddr,
    ) -> Response<ProxyBody> {
        let Some(portal) = req
            .headers()
            .get(HOST)
            .and_then(|v| v.to_str().ok())
            .and_then(|h| self.portal_for_host(h))
        else {
            return status_response(StatusCode::NOT_FOUND, "no login portal configured");
        };
        let Portal {
            auth: settings,
            fqdn: portal,
            network,
        } = portal;
        if scheme != "https" {
            let pq = req.uri().path_and_query().map_or("/", |p| p.as_str());
            return permanent_redirect(&format!("https://{portal}{pq}"));
        }
        let params = form_urlencoded_parse(req.uri().query().unwrap_or(""));
        let callback = params.get("callback").map(String::as_str);
        let safe_callback = callback.and_then(|c| parse_callback(c, routes, &network));
        let offer = self.remember_offer(
            &settings,
            &network,
            peer,
            safe_callback.as_ref().map(|c| c.url.as_str()),
        );

        let mut resp = match (req.method(), req.uri().path()) {
            (&Method::GET | &Method::HEAD, "/") => {
                self.portal_home(req, &settings, &portal, offer.as_ref())
                    .await
            }
            (&Method::GET | &Method::HEAD, "/login") => {
                self.portal_login(
                    req,
                    &settings,
                    &portal,
                    callback,
                    safe_callback,
                    offer.as_ref(),
                )
                .await
            }
            (&Method::GET | &Method::HEAD, "/start") => {
                let wants = params.get("remember").is_some_and(|v| v == "1");
                let confirmed = params.get("confirm").is_some_and(|v| v == "1");
                // The checkbox is only a request: whether this address may be
                // saved is decided again here, from the socket peer.
                match offer.as_ref().filter(|_| wants) {
                    Some(offer) if !confirmed => confirm_remember_page(&settings, offer),
                    remember => {
                        self.start_login(
                            &settings,
                            &portal,
                            safe_callback.as_ref().map(|c| c.url.clone()),
                            remember.map(|o| o.ip.to_string()),
                        )
                        .await
                    }
                }
            }
            (&Method::GET | &Method::HEAD, "/callback") => {
                self.finish_login(req, routes, &settings, &portal, &network, peer)
                    .await
            }
            (&Method::GET | &Method::HEAD | &Method::POST, "/logout") => {
                self.portal_logout(req, &portal, safe_callback.map(|c| c.url), &network, peer)
                    .await
            }
            _ => status_response(StatusCode::NOT_FOUND, "not found"),
        };
        resp.headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        resp
    }

    async fn portal_home(
        &self,
        req: &Request<Incoming>,
        settings: &ProxyAuthConfig,
        portal: &str,
        offer: Option<&RememberOffer>,
    ) -> Response<ProxyBody> {
        match self
            .session(req.headers(), PORTAL_COOKIE_NAME, portal)
            .await
        {
            Some((_, s)) => signed_in_page(settings, &s.username),
            None => login_page(settings, None, "/start", None, offer),
        }
    }

    async fn portal_login(
        &self,
        req: &Request<Incoming>,
        settings: &ProxyAuthConfig,
        portal: &str,
        raw_callback: Option<&str>,
        callback: Option<Callback>,
        offer: Option<&RememberOffer>,
    ) -> Response<ProxyBody> {
        if raw_callback.is_some() && callback.is_none() {
            return message_page(
                StatusCode::BAD_REQUEST,
                settings,
                "Unknown destination",
                "The page you were sent from is not protected by this login.",
                None,
            );
        }
        match self
            .session(req.headers(), PORTAL_COOKIE_NAME, portal)
            .await
        {
            Some((key, session)) => match callback {
                Some(cb) => self.hand_over(&cb, &key, &session).await,
                None => signed_in_page(settings, &session.username),
            },
            None => {
                let start = match &callback {
                    Some(cb) => format!("/start?callback={}", urlencode(&cb.url)),
                    None => "/start".to_string(),
                };
                login_page(
                    settings,
                    callback.as_ref().map(|c| c.host.as_str()),
                    &start,
                    None,
                    offer,
                )
            }
        }
    }

    /// Mints a one-time token for `cb`'s host and redirects to its cookie
    /// endpoint. The session it will create ends with the portal session.
    async fn hand_over(
        &self,
        cb: &Callback,
        session_key: &str,
        session: &ProxySessionRecord,
    ) -> Response<ProxyBody> {
        let token = random_id();
        let now = now_unix() as i64;
        let stored = self
            .submit(RaftRequest::PutProxyToken {
                key: hash_id(&token),
                token: ProxyTokenRecord {
                    host: normalize_host(&cb.host),
                    subject: session.subject.clone(),
                    username: session.username.clone(),
                    groups: session.groups.clone(),
                    session_expires_at: session.expires_at,
                    expires_at: now + TOKEN_TTL.as_secs() as i64,
                    parent: session_key.to_string(),
                },
                now,
            })
            .await;
        if let Err(e) = stored {
            warn!(error = %e, "cannot store a proxy login token");
            return bad_gateway("cannot store login token");
        }
        let mut resp = redirect_response(&format!(
            "{}://{}{COOKIE_PATH}?token={token}&redirect={}",
            cb.scheme,
            cb.host,
            urlencode(&cb.path_and_query)
        ));
        resp.headers_mut()
            .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
        resp
    }

    async fn portal_logout(
        &self,
        req: &Request<Incoming>,
        portal: &str,
        callback: Option<String>,
        network: &str,
        peer: IpAddr,
    ) -> Response<ProxyBody> {
        // Signing out also forgets this address, or "sign out" on a shared
        // machine would leave the door open.
        let _ = self
            .submit(RaftRequest::RemoveProxySession {
                key: trusted_key(network, peer),
            })
            .await;
        if let Some(id) = extract_cookie(req.headers(), PORTAL_COOKIE_NAME) {
            if id.len() == ID_LEN {
                let _ = self
                    .submit(RaftRequest::RemoveProxySession { key: hash_id(&id) })
                    .await;
            }
        }
        let to = match callback {
            Some(url) => format!("https://{portal}/login?callback={}", urlencode(&url)),
            None => "/".to_string(),
        };
        let mut resp = redirect_response(&to);
        append_cookie(&mut resp, PORTAL_COOKIE_NAME, "", "/", Some(0), true);
        resp
    }

    async fn start_login(
        &self,
        settings: &ProxyAuthConfig,
        portal: &str,
        callback: Option<String>,
        remember_ip: Option<String>,
    ) -> Response<ProxyBody> {
        let Some(key) = self.key() else {
            return status_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "this node holds no cluster key",
            );
        };
        let metadata = match self.metadata(&settings.issuer).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "oidc discovery failed while starting a login");
                return bad_gateway("oidc discovery failed");
            }
        };
        let Ok(redirect) = RedirectUrl::new(format!("https://{portal}/callback")) else {
            return bad_gateway("invalid redirect url");
        };
        let secret = self.client_secret().await;
        let client = build_client!(metadata, settings, secret, redirect);

        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let mut request = client.authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        );
        for scope in &settings.scopes {
            request = request.add_scope(Scope::new(scope.clone()));
        }
        let (auth_url, csrf_token, nonce) = request.set_pkce_challenge(pkce_challenge).url();

        let pending = PendingLogin {
            state: csrf_token.secret().clone(),
            nonce: nonce.secret().clone(),
            pkce_verifier: pkce_verifier.secret().clone(),
            callback: callback.unwrap_or_default(),
            started_at_unix: now_unix(),
            remember_ip,
        };
        let Some(sealed) = seal_login(&key, &pending) else {
            return bad_gateway("cannot seal login state");
        };
        let mut resp = redirect_response(auth_url.as_str());
        append_cookie(
            &mut resp,
            LOGIN_COOKIE_NAME,
            &sealed,
            LOGIN_COOKIE_PATH,
            Some(LOGIN_TIMEOUT.as_secs()),
            true,
        );
        resp
    }

    async fn finish_login(
        &self,
        req: &Request<Incoming>,
        routes: &RouteTable,
        settings: &ProxyAuthConfig,
        portal: &str,
        network: &str,
        peer: IpAddr,
    ) -> Response<ProxyBody> {
        let Some(key) = self.key() else {
            return status_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "this node holds no cluster key",
            );
        };
        let params = form_urlencoded_parse(req.uri().query().unwrap_or(""));
        if let Some(err) = params.get("error") {
            warn!(error = %err, "identity provider refused the proxy login");
            return message_page(
                StatusCode::UNAUTHORIZED,
                settings,
                "Sign-in refused",
                "The identity provider did not let you in.",
                Some(("Try again", "/")),
            );
        }
        let (Some(code), Some(state)) = (params.get("code").cloned(), params.get("state").cloned())
        else {
            return status_response(StatusCode::BAD_REQUEST, "missing code or state");
        };
        let unknown = || {
            message_page(
                StatusCode::UNAUTHORIZED,
                settings,
                "Sign-in expired",
                "This sign-in attempt is unknown or took too long.",
                Some(("Start again", "/")),
            )
        };
        let Some(pending) =
            extract_cookie(req.headers(), LOGIN_COOKIE_NAME).and_then(|v| open_login(&key, &v))
        else {
            return unknown();
        };
        if !constant_time_eq(pending.state.as_bytes(), state.as_bytes())
            || now_unix().saturating_sub(pending.started_at_unix) > LOGIN_TIMEOUT.as_secs()
        {
            return unknown();
        }

        let identity = match self.exchange(settings, portal, code, &pending).await {
            Ok(i) => i,
            Err(resp) => return *resp,
        };

        let now = now_unix() as i64;
        let record = ProxySessionRecord {
            host: portal.to_string(),
            subject: identity.subject,
            username: identity.username,
            groups: identity.groups,
            expires_at: now + settings.session_ttl.as_secs().min(i64::MAX as u64 / 2) as i64,
            parent: String::new(),
        };
        let id = match self.create_session(record.clone()).await {
            Ok(id) => id,
            Err(e) => {
                warn!(error = %e, "cannot store the portal session");
                return bad_gateway("cannot store session");
            }
        };

        // Save the address only if it is the one the visitor agreed to and is
        // still allowed to be saved: the browser may have moved networks, or
        // the network's setting may have changed, during the round trip.
        if pending.remember_ip.as_deref() == Some(peer.to_canonical().to_string().as_str())
            && self.can_remember(network, peer)
        {
            let saved = ProxySessionRecord {
                host: trusted_host(network, peer),
                expires_at: now
                    + settings.remember_ip_ttl.as_secs().min(i64::MAX as u64 / 2) as i64,
                ..record.clone()
            };
            if let Err(e) = self
                .submit(RaftRequest::PutProxySession {
                    key: trusted_key(network, peer),
                    session: saved,
                    now,
                })
                .await
            {
                warn!(error = %e, "cannot save the visitor's ip address");
            }
        }

        // The callback was validated when the login started, but routes may
        // have changed in the meantime; check again.
        let mut resp = match parse_callback(&pending.callback, routes, network) {
            Some(cb) => self.hand_over(&cb, &hash_id(&id), &record).await,
            None => redirect_response("/"),
        };
        append_cookie(
            &mut resp,
            PORTAL_COOKIE_NAME,
            &id,
            "/",
            Some((record.expires_at - now).max(1) as u64),
            true,
        );
        append_cookie(
            &mut resp,
            LOGIN_COOKIE_NAME,
            "",
            LOGIN_COOKIE_PATH,
            Some(0),
            true,
        );
        resp
    }

    /// Code exchange and id token verification. Err is a ready response.
    async fn exchange(
        &self,
        settings: &ProxyAuthConfig,
        portal: &str,
        code: String,
        pending: &PendingLogin,
    ) -> Result<Identity, Box<Response<ProxyBody>>> {
        let metadata = self.metadata(&settings.issuer).await.map_err(|e| {
            warn!(error = %e, "oidc discovery failed while finishing a login");
            Box::new(bad_gateway("oidc discovery failed"))
        })?;
        let redirect = RedirectUrl::new(format!("https://{portal}/callback"))
            .map_err(|_| Box::new(bad_gateway("invalid redirect url")))?;
        let secret = self.client_secret().await;
        let client = build_client!(metadata, settings, secret, redirect);

        let token_request = client
            .exchange_code(AuthorizationCode::new(code))
            .map_err(|e| {
                warn!(error = %e, "oidc token endpoint not configured");
                Box::new(bad_gateway("token endpoint not configured"))
            })?;
        let token_response = token_request
            .set_pkce_verifier(PkceCodeVerifier::new(pending.pkce_verifier.clone()))
            .request_async(&self.http)
            .await
            .map_err(|e| {
                warn!(error = %e, "oidc token exchange failed");
                Box::new(status_response(
                    StatusCode::UNAUTHORIZED,
                    "token exchange failed",
                ))
            })?;
        let id_token = token_response.id_token().ok_or_else(|| {
            Box::new(status_response(
                StatusCode::UNAUTHORIZED,
                "provider did not return an id token",
            ))
        })?;
        let nonce = Nonce::new(pending.nonce.clone());
        let subject = match id_token
            .claims(&client.id_token_verifier(), &nonce)
            .map(|c| c.subject().as_str().to_string())
        {
            Ok(s) => s,
            Err(first) => {
                // A rotated signing key looks exactly like a bad signature
                // against the cached jwks: refetch discovery once and retry.
                let retried = match self.refresh_metadata(&settings.issuer).await {
                    Ok(fresh) => {
                        let client = build_client!(fresh, settings, secret, redirect);
                        let verifier = client.id_token_verifier();
                        let result = id_token
                            .claims(&verifier, &nonce)
                            .map(|c| c.subject().as_str().to_string())
                            .map_err(|e| e.to_string());
                        result
                    }
                    Err(e) => Err(e),
                };
                retried.map_err(|second| {
                    warn!(first = %first, second = %second, "oidc id token verification failed");
                    Box::new(status_response(
                        StatusCode::UNAUTHORIZED,
                        "id token verification failed",
                    ))
                })?
            }
        };

        // `claims` verified the signature; the payload is now re-read loosely
        // for the configurable claims `CoreClient` does not model.
        let payload = decode_jwt_payload(&id_token.to_string()).unwrap_or_default();
        Ok(Identity {
            username: payload
                .get(&settings.username_claim)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| subject.clone()),
            groups: claim_strings(payload.get(&settings.groups_claim)),
            subject,
        })
    }
}

/// A validated destination the portal may send a browser back to.
#[derive(Debug, PartialEq, Eq)]
struct Callback {
    scheme: String,
    /// Authority as written (host and optional port).
    host: String,
    path_and_query: String,
    /// The whole thing, normalized.
    url: String,
}

/// Accepts a callback only when it names a host whose matching route carries
/// the `authenticated` middleware on this portal's `network`, so the portal is
/// not an open redirect and never signs anyone in on a network it does not
/// serve, and only when its path is an ordinary page of that app.
fn parse_callback(raw: &str, routes: &RouteTable, network: &str) -> Option<Callback> {
    if raw.is_empty() || raw.len() > 4096 || raw.contains(['\r', '\n', '\\']) {
        return None;
    }
    let uri: Uri = raw.parse().ok()?;
    let scheme = uri.scheme_str()?;
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let authority = uri.authority()?;
    if authority.as_str().contains('@') {
        return None;
    }
    let path_and_query = uri.path_and_query().map_or("/", |p| p.as_str()).to_string();
    if !is_safe_return(&path_and_query) {
        return None;
    }
    let host = authority.as_str().to_ascii_lowercase();
    let route = routes.lookup(&host, uri.path())?;
    if route.options.network.as_deref() != Some(network) {
        return None;
    }
    route
        .middlewares
        .iter()
        .any(|m| matches!(m, cl_config::Middleware::Authenticated(_)))
        .then(|| Callback {
            url: format!("{scheme}://{host}{path_and_query}"),
            scheme: scheme.to_string(),
            host,
            path_and_query,
        })
}

/// Does a session with `groups` satisfy the policy?
fn permitted(policy: &Authenticated, groups: &[String]) -> bool {
    policy.groups.is_empty() || policy.groups.iter().any(|g| groups.contains(g))
}

/// Only a navigation gets a redirect to the login page; an API client or a
/// non-GET request gets a plain 401 instead of a surprise HTML round trip.
fn wants_redirect(req: &Request<Incoming>) -> bool {
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return false;
    }
    match req.headers().get(ACCEPT).and_then(|v| v.to_str().ok()) {
        None => true,
        Some(accept) => accept.contains("text/html"),
    }
}

/// The path and query the user actually asked for.
fn return_target(uri: &Uri) -> String {
    uri.path_and_query()
        .map(|p| p.as_str())
        .filter(|p| is_safe_return(p))
        .unwrap_or("/")
        .to_string()
}

/// Accepts only a single-slash absolute path, since the value ends up in a
/// `Location` header and anything else (`//evil`, `/\evil`, header
/// injection, the gate's own reserved paths) is an open redirect or a loop.
fn is_safe_return(r: &str) -> bool {
    r.starts_with('/')
        && !r.starts_with("//")
        && !r.starts_with("/\\")
        && !r.contains(['\\', '\r', '\n'])
        && !r.starts_with(RESERVED_PREFIX)
}

fn claim_strings(value: Option<&serde_json::Value>) -> Vec<String> {
    match value {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn decode_jwt_payload(compact: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let payload = compact.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice::<serde_json::Value>(&raw)
        .ok()?
        .as_object()
        .cloned()
}

// ---- responses ----

fn redirect_response(location: &str) -> Response<ProxyBody> {
    redirect_with(StatusCode::FOUND, location)
}

fn permanent_redirect(location: &str) -> Response<ProxyBody> {
    redirect_with(StatusCode::PERMANENT_REDIRECT, location)
}

fn redirect_with(status: StatusCode, location: &str) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .header(LOCATION, location)
        .body(empty_body())
        .unwrap_or_else(|_| Response::new(empty_body()))
}

fn status_response(status: StatusCode, message: &str) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .body(text_body(message.to_string()))
        .unwrap_or_else(|_| Response::new(empty_body()))
}

fn bad_gateway(reason: impl std::fmt::Display) -> Response<ProxyBody> {
    status_response(StatusCode::BAD_GATEWAY, &format!("authenticated: {reason}"))
}

/// `max_age` of `Some(0)` expires the cookie.
fn append_cookie(
    resp: &mut Response<ProxyBody>,
    name: &str,
    value: &str,
    path: &str,
    max_age: Option<u64>,
    secure: bool,
) {
    let mut cookie = format!("{name}={value}; Path={path}; HttpOnly; SameSite=Lax");
    if let Some(age) = max_age {
        cookie.push_str(&format!("; Max-Age={age}"));
    }
    if secure {
        cookie.push_str("; Secure");
    }
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut().append(SET_COOKIE, v);
    }
}

fn extract_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(';'))
        .find_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k == name).then(|| v.to_string())
        })
}

/// Removes the gate's own cookies from the `Cookie` header(s) before the
/// request is forwarded, so a backend cannot capture a session and replay it.
/// Other cookies are kept; the header is dropped if nothing is left.
pub fn strip_gate_cookies(headers: &mut HeaderMap) {
    if !headers.contains_key(COOKIE) {
        return;
    }
    let kept: Vec<String> = headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(';'))
        .map(str::trim)
        .filter(|kv| !kv.is_empty())
        .filter(|kv| {
            let name = kv.split_once('=').map_or(*kv, |(k, _)| k);
            name != COOKIE_NAME && name != LOGIN_COOKIE_NAME && name != PORTAL_COOKIE_NAME
        })
        .map(str::to_string)
        .collect();
    headers.remove(COOKIE);
    if kept.is_empty() {
        return;
    }
    if let Ok(v) = HeaderValue::from_str(&kept.join("; ")) {
        headers.insert(COOKIE, v);
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn form_urlencoded_parse(query: &str) -> HashMap<String, String> {
    serde_urlencoded::from_str(query).unwrap_or_default()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Length-aware constant-time byte comparison, for the csrf state.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Seals the login state into a cookie-safe (URL-safe base64) string.
fn seal_login(key: &ClusterKey, pending: &PendingLogin) -> Option<String> {
    let json = serde_json::to_vec(pending).ok()?;
    let sealed = Sealed::seal(key, LOGIN_SEAL_CONTEXT, &json).ok()?;
    // `Sealed` blobs are standard base64, whose `+`, `/` and `=` are not safe
    // in a cookie value, so re-encode the raw bytes.
    let raw = base64::engine::general_purpose::STANDARD
        .decode(sealed.blob)
        .ok()?;
    Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw))
}

fn open_login(key: &ClusterKey, value: &str) -> Option<PendingLogin> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .ok()?;
    let blob = base64::engine::general_purpose::STANDARD.encode(raw);
    let plaintext = Sealed::from_blob(blob).open(key, LOGIN_SEAL_CONTEXT).ok()?;
    serde_json::from_slice(&plaintext).ok()
}

/// Lowercases the host and strips any port, so a session is bound to the same
/// name however the browser spelled the authority.
fn normalize_host(host: &str) -> String {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest).to_string();
    }
    host.split(':').next().unwrap_or("").to_string()
}

/// Sessions and tokens are stored under this hash so the replicated state
/// never holds a usable cookie.
fn hash_id(id: &str) -> String {
    Sha256::digest(id.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn random_id() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut out = String::with_capacity(ID_LEN);
    while out.len() < ID_LEN {
        let mut buf = [0u8; 32];
        rand::fill(&mut buf);
        for byte in buf {
            if out.len() == ID_LEN {
                break;
            }
            // Rejection sampling, so every character is equally likely.
            if (byte as usize) < (256 / ALPHABET.len()) * ALPHABET.len() {
                out.push(ALPHABET[byte as usize % ALPHABET.len()] as char);
            }
        }
    }
    out
}

// ---- pages ----

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The colours a page is drawn with.
struct Colors {
    bg: &'static str,
    surface: &'static str,
    line: &'static str,
    ink: &'static str,
    muted: &'static str,
    brand: String,
    on_brand: &'static str,
    pop: &'static str,
    third: &'static str,
}

/// Palette plus the optional accent override. The accent is re-validated
/// here so a stale stored value can never break out of the stylesheet.
fn colors(settings: &ProxyAuthConfig) -> Colors {
    let (bg, surface, line, ink, muted, brand, pop, third) = match settings.palette {
        PortalPalette::Grape => (
            "#150f1f", "#1f1830", "#3a2f57", "#f6f0ff", "#b9acd6", "#ff5cae", "#c8f542", "#2fd0a8",
        ),
        PortalPalette::Sunset => (
            "#1c1008", "#2a1a0e", "#4d3220", "#fff3e6", "#d6bfa6", "#ff8a3d", "#5cc8ff", "#ffd23d",
        ),
        PortalPalette::Ocean => (
            "#08161f", "#0f2533", "#1f4256", "#e8f6ff", "#9cc2d6", "#2fd0e8", "#ff7a5c", "#ffd23d",
        ),
        PortalPalette::Forest => (
            "#0c1a12", "#12281b", "#24472f", "#eefaf0", "#a6c9b0", "#ffd23d", "#ff5cae", "#7be0a0",
        ),
    };
    let accent = settings.accent.as_deref().and_then(valid_hex);
    Colors {
        bg,
        surface,
        line,
        ink,
        muted,
        on_brand: accent.map_or("#1a0f12", on_color),
        brand: accent.map_or_else(|| brand.to_string(), str::to_string),
        pop,
        third,
    }
}

/// `#rgb` or `#rrggbb`, nothing else.
fn valid_hex(value: &str) -> Option<&str> {
    let hex = value.strip_prefix('#')?;
    (matches!(hex.len(), 3 | 6) && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(value)
}

/// Dark or light text, whichever reads on `hex`.
fn on_color(hex: &str) -> &'static str {
    let digits = hex.trim_start_matches('#');
    let expand = |i: usize| -> f32 {
        let v = if digits.len() == 3 {
            u8::from_str_radix(&digits[i..=i].repeat(2), 16)
        } else {
            u8::from_str_radix(&digits[i * 2..i * 2 + 2], 16)
        }
        .unwrap_or(0);
        f32::from(v) / 255.0
    };
    let luminance = 0.2126 * expand(0) + 0.7152 * expand(1) + 0.0722 * expand(2);
    if luminance > 0.55 {
        "#1a0f12"
    } else {
        "#ffffff"
    }
}

fn page(
    status: StatusCode,
    settings: &ProxyAuthConfig,
    heading: &str,
    body: &str,
) -> Response<ProxyBody> {
    let c = colors(settings);
    let html = format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex">
<title>{title}</title>
<style>
:root{{--bg:{bg};--surface:{surface};--line:{line};--ink:{ink};--muted:{muted};
--brand:{brand};--on-brand:{on_brand};--pop:{pop};--third:{third}}}
*{{box-sizing:border-box}}
body{{margin:0;min-height:100vh;display:grid;place-items:center;padding:24px;overflow:hidden;position:relative;
font:16px/1.5 ui-rounded,"Arial Rounded MT Bold",system-ui,-apple-system,"Segoe UI",sans-serif;
color:var(--ink);background:var(--bg)}}
.shape{{position:absolute;border:3px solid var(--ink);animation:bob 7s ease-in-out infinite}}
.s1{{width:340px;height:340px;border-radius:50%;background:var(--brand);left:-90px;top:-80px}}
.s2{{width:190px;height:190px;border-radius:50%;background:var(--pop);right:6%;bottom:-60px;animation-delay:-2s}}
.s3{{width:140px;height:140px;border-radius:28px;background:var(--surface);right:-30px;top:14%;transform:rotate(14deg);animation-delay:-4s}}
.s4{{width:84px;height:84px;border-radius:50%;border-color:var(--muted);background:none;left:10%;bottom:12%;animation-delay:-1s}}
.s5{{width:70px;height:70px;background:var(--third);left:38%;top:7%;transform:rotate(-12deg);animation-delay:-3s}}
.s6{{width:0;height:0;border:0;border-left:44px solid transparent;border-right:44px solid transparent;
border-bottom:76px solid var(--pop);right:24%;top:9%;animation-delay:-5s}}
@keyframes bob{{50%{{translate:0 -14px}}}}
@media (prefers-reduced-motion:reduce){{.shape{{animation:none}}}}
main{{position:relative;width:100%;max-width:420px;padding:36px 32px 32px;border-radius:28px;
background:var(--surface);border:3px solid var(--ink);box-shadow:8px 8px 0 var(--pop);text-align:center}}
.mark{{position:relative;width:56px;height:56px;margin:-62px auto 18px;border-radius:18px;
background:var(--brand);border:3px solid var(--ink);box-shadow:4px 4px 0 var(--ink)}}
.mark::after{{content:"";position:absolute;width:20px;height:20px;border-radius:50%;
background:var(--pop);border:3px solid var(--ink);right:-12px;bottom:-12px}}
h1{{margin:0 0 8px;font-size:28px;line-height:1.15;font-weight:800;letter-spacing:-.02em}}
p{{margin:0 0 24px;color:var(--muted)}}
code{{color:var(--ink);background:var(--bg);padding:2px 8px;border-radius:999px;
border:2px solid var(--line);word-break:break-all;font-size:.92em}}
a.btn{{display:block;padding:14px 18px;border-radius:16px;background:var(--brand);color:var(--on-brand);
border:3px solid var(--ink);box-shadow:5px 5px 0 var(--ink);font-weight:800;font-size:17px;text-decoration:none;
transition:transform .08s,box-shadow .08s}}
button.btn{{display:block;width:100%;font:inherit;cursor:pointer}}
.remember{{display:block;margin:0 0 16px;font-size:14px;text-align:left}}
.remember input{{margin-right:8px;width:18px;height:18px;vertical-align:middle}}
a.btn:hover,button.btn:hover{{transform:translate(-1px,-1px);box-shadow:6px 6px 0 var(--ink)}}
a.btn:active,button.btn:active{{transform:translate(4px,4px);box-shadow:1px 1px 0 var(--ink)}}
a.btn:focus-visible,button.btn:focus-visible{{outline:3px solid var(--pop);outline-offset:3px}}
.note{{margin:20px 0 0;font-size:13px;color:var(--muted)}}
@media (max-width:520px){{.s1{{width:240px;height:240px}}.s3,.s6{{display:none}}}}
</style></head><body>
<div aria-hidden="true"><i class="shape s1"></i><i class="shape s2"></i><i class="shape s3"></i>
<i class="shape s4"></i><i class="shape s5"></i><i class="shape s6"></i></div>
<main>
<div class="mark" aria-hidden="true"></div>
<h1>{heading}</h1>
{body}
</main></body></html>"#,
        title = html_escape(heading),
        bg = c.bg,
        surface = c.surface,
        line = c.line,
        ink = c.ink,
        muted = c.muted,
        brand = c.brand,
        on_brand = c.on_brand,
        pop = c.pop,
        third = c.third,
        heading = html_escape(heading),
    );
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/html; charset=utf-8")
        .header(
            "content-security-policy",
            "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
        )
        .header("x-frame-options", "DENY")
        .header("referrer-policy", "no-referrer")
        .body(text_body(html))
        .unwrap_or_else(|_| Response::new(empty_body()))
}

fn login_page(
    settings: &ProxyAuthConfig,
    app_host: Option<&str>,
    start_href: &str,
    error: Option<&str>,
    offer: Option<&RememberOffer>,
) -> Response<ProxyBody> {
    let lead = match app_host {
        Some(host) => format!("to open <code>{}</code>", html_escape(host)),
        None => "to continue".to_string(),
    };
    let error = error
        .map(|e| format!("<p>{}</p>", html_escape(e)))
        .unwrap_or_default();
    page(
        StatusCode::OK,
        settings,
        &settings.title,
        &match offer {
            // A form rather than a link, so the checkbox travels with the
            // click. No script: the page's CSP forbids it, and the
            // confirmation is the next page.
            Some(offer) => format!(
                "<p>Use your account {lead}.</p>{error}\
                 <form method=\"get\" action=\"/start\">{}\
                 <label class=\"remember\"><input type=\"checkbox\" name=\"remember\" value=\"1\"> \
                 Remember this IP address (<code>{}</code>) for {}</label>\
                 <button class=\"btn\" type=\"submit\">Continue</button></form>",
                callback_input(offer.callback.as_deref()),
                offer.ip,
                html_escape(&offer.lasts),
            ),
            None => format!(
                "<p>Use your account {lead}.</p>{error}<a class=\"btn\" href=\"{}\">Continue</a>",
                html_escape(start_href)
            ),
        },
    )
}

fn callback_input(callback: Option<&str>) -> String {
    callback
        .map(|c| {
            format!(
                "<input type=\"hidden\" name=\"callback\" value=\"{}\">",
                html_escape(c)
            )
        })
        .unwrap_or_default()
}

/// The confirmation step for "remember this IP address".
fn confirm_remember_page(settings: &ProxyAuthConfig, offer: &RememberOffer) -> Response<ProxyBody> {
    let skip = match &offer.callback {
        Some(cb) => format!("/start?callback={}", urlencode(cb)),
        None => "/start".to_string(),
    };
    page(
        StatusCode::OK,
        settings,
        "Remember this IP address?",
        &format!(
            "<p>Anyone using <code>{ip}</code> on the {network} network will open protected apps \
             as you, without signing in, for {lasts}. Only do this on a connection you trust, \
             not a shared or public one. Signing out here forgets it.</p>\
             <form method=\"get\" action=\"/start\">{cb}\
             <input type=\"hidden\" name=\"remember\" value=\"1\">\
             <input type=\"hidden\" name=\"confirm\" value=\"1\">\
             <button class=\"btn\" type=\"submit\">Remember it and continue</button></form>\
             <p class=\"note\"><a href=\"{skip}\">Continue without remembering</a></p>",
            ip = offer.ip,
            network = html_escape(&offer.network),
            lasts = html_escape(&offer.lasts),
            cb = callback_input(offer.callback.as_deref()),
            skip = html_escape(&skip),
        ),
    )
}

fn signed_in_page(settings: &ProxyAuthConfig, username: &str) -> Response<ProxyBody> {
    page(
        StatusCode::OK,
        settings,
        "You are signed in",
        &format!(
            "<p>Signed in as <code>{}</code>.</p><a class=\"btn\" href=\"/logout\">Sign out</a>",
            html_escape(username)
        ),
    )
}

fn forbidden_page(
    settings: &ProxyAuthConfig,
    username: &str,
    portal: &str,
    return_to: &str,
) -> Response<ProxyBody> {
    let switch = format!("https://{portal}/logout?callback={}", urlencode(return_to));
    page(
        StatusCode::FORBIDDEN,
        settings,
        "No access",
        &format!(
            "<p><code>{}</code> is not in a group allowed to open this page.</p>\
             <a class=\"btn\" href=\"{}\">Use another account</a>",
            html_escape(username),
            html_escape(&switch)
        ),
    )
}

fn message_page(
    status: StatusCode,
    settings: &ProxyAuthConfig,
    heading: &str,
    text: &str,
    action: Option<(&str, &str)>,
) -> Response<ProxyBody> {
    let button = action
        .map(|(label, href)| {
            format!(
                "<a class=\"btn\" href=\"{}\">{}</a>",
                html_escape(href),
                html_escape(label)
            )
        })
        .unwrap_or_default();
    page(
        status,
        settings,
        heading,
        &format!("<p>{}</p>{button}", html_escape(text)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::Route;

    fn routes() -> RouteTable {
        let protected = |host: &str| {
            let mut route = Route::new(
                host,
                None,
                true,
                vec![cl_config::Middleware::Authenticated(
                    Authenticated::default(),
                )],
                false,
                "t",
                vec![],
            );
            route.options.network = Some("public".into());
            route
        };
        let open = Route::new("open.example.com", None, true, vec![], false, "t", vec![]);
        let mut private = protected("lan.example.com");
        private.options.network = Some("private".into());
        RouteTable::new(vec![protected("app.example.com"), private, open])
    }

    #[test]
    fn callback_must_name_a_protected_host() {
        let routes = routes();
        let cb = parse_callback("https://app.example.com/library?x=1", &routes, "public").unwrap();
        assert_eq!(cb.host, "app.example.com");
        assert_eq!(cb.path_and_query, "/library?x=1");
        assert!(parse_callback("https://open.example.com/", &routes, "public").is_none());
        assert!(parse_callback("https://evil.example.net/", &routes, "public").is_none());
    }

    #[test]
    fn callback_must_be_on_the_portals_own_network() {
        let routes = routes();
        assert!(parse_callback("https://lan.example.com/", &routes, "public").is_none());
        assert!(parse_callback("https://lan.example.com/", &routes, "private").is_some());
        assert!(parse_callback("https://app.example.com/", &routes, "private").is_none());
    }

    #[test]
    fn callback_rejects_tricks() {
        let routes = routes();
        for bad in [
            "",
            "javascript:alert(1)",
            "/relative",
            "https://user@app.example.com/",
            "https://app.example.com/.well-known/clouduler-cookie?token=x",
            "https://app.example.com//evil.com",
            "ftp://app.example.com/",
        ] {
            assert!(parse_callback(bad, &routes, "public").is_none(), "{bad}");
        }
    }

    #[test]
    fn callback_host_is_lowercased() {
        let cb = parse_callback("https://APP.example.com/", &routes(), "public").unwrap();
        assert_eq!(cb.host, "app.example.com");
    }

    #[test]
    fn groups_restrict_only_when_listed() {
        let any = Authenticated::default();
        assert!(permitted(&any, &[]));
        let family = Authenticated {
            groups: vec!["family".into(), "admins".into()],
            allow_internal: true,
        };
        assert!(!permitted(&family, &[]));
        assert!(!permitted(&family, &["guests".to_string()]));
        assert!(permitted(&family, &["x".to_string(), "admins".to_string()]));
    }

    #[test]
    fn group_claims_accept_a_string_or_a_list() {
        let list = serde_json::json!(["a", "b", 3]);
        assert_eq!(claim_strings(Some(&list)), vec!["a", "b"]);
        let one = serde_json::json!("solo");
        assert_eq!(claim_strings(Some(&one)), vec!["solo"]);
        assert!(claim_strings(None).is_empty());
    }

    #[test]
    fn ids_are_distinct_and_hashes_are_stable() {
        let a = random_id();
        assert_eq!(a.len(), ID_LEN);
        assert_ne!(a, random_id());
        assert_eq!(hash_id("x"), hash_id("x"));
        assert_ne!(hash_id("x"), hash_id("y"));
        assert_eq!(hash_id("x").len(), 64);
    }

    #[test]
    fn host_case_port_and_trailing_dot_do_not_matter() {
        assert_eq!(normalize_host("A.Example.com:8443"), "a.example.com");
        assert_eq!(normalize_host("a.example.com."), "a.example.com");
    }

    #[test]
    fn return_target_hides_reserved_paths() {
        let uri: Uri = "/.well-known/clouduler-cookie?token=x".parse().unwrap();
        assert_eq!(return_target(&uri), "/");
        let uri: Uri = "/library?x=1".parse().unwrap();
        assert_eq!(return_target(&uri), "/library?x=1");
    }

    #[test]
    fn form_urlencoded_parse_decodes_percent_and_plus() {
        let params = form_urlencoded_parse("code=abc&state=x%2Fy+z");
        assert_eq!(params.get("code").map(String::as_str), Some("abc"));
        assert_eq!(params.get("state").map(String::as_str), Some("x/y z"));
    }

    #[test]
    fn safe_return_accepts_local_paths_only() {
        assert!(is_safe_return("/a?b=1"));
        for bad in [
            "//evil.com",
            "/\\evil.com",
            "https://evil.com",
            "evil",
            "/a\\b",
            "/a\r\nb",
        ] {
            assert!(!is_safe_return(bad), "{bad}");
        }
    }

    #[test]
    fn login_state_round_trips_and_is_cookie_safe() {
        let key = ClusterKey::generate();
        let pending = PendingLogin {
            state: "s".into(),
            nonce: "n".into(),
            pkce_verifier: "v".into(),
            callback: "https://app.example.com/x?y=1".into(),
            started_at_unix: 5,
            remember_ip: Some("192.168.1.5".into()),
        };
        let sealed = seal_login(&key, &pending).unwrap();
        assert!(sealed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        let back = open_login(&key, &sealed).unwrap();
        assert_eq!(back.state, "s");
        assert_eq!(back.callback, "https://app.example.com/x?y=1");
        assert_eq!(back.remember_ip.as_deref(), Some("192.168.1.5"));
        assert!(open_login(&ClusterKey::generate(), &sealed).is_none());
        assert!(open_login(&key, "garbage").is_none());
    }

    #[test]
    fn constant_time_eq_compares_content_and_length() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn strip_gate_cookies_keeps_other_cookies() {
        let mut h = HeaderMap::new();
        h.insert(
            COOKIE,
            HeaderValue::from_static(
                "a=1; cl_proxy_auth=zzz; cl_proxy_portal=p; cl_proxy_login=q; b=2",
            ),
        );
        strip_gate_cookies(&mut h);
        assert_eq!(h.get(COOKIE).unwrap(), "a=1; b=2");
    }

    #[test]
    fn strip_gate_cookies_drops_an_emptied_header() {
        let mut h = HeaderMap::new();
        h.insert(COOKIE, HeaderValue::from_static("cl_proxy_auth=zzz"));
        strip_gate_cookies(&mut h);
        assert!(h.get(COOKIE).is_none());
    }

    #[test]
    fn pages_escape_what_they_show() {
        let s = html_escape("<script>\"x\"&'");
        assert_eq!(s, "&lt;script&gt;&quot;x&quot;&amp;&#39;");
        assert_eq!(valid_hex("#abc"), Some("#abc"));
        assert_eq!(valid_hex("red;}</style>"), None);
        assert_eq!(valid_hex("#12345"), None);
    }

    #[test]
    fn text_on_the_accent_stays_readable() {
        assert_eq!(on_color("#ffffff"), "#1a0f12");
        assert_eq!(on_color("#000"), "#ffffff");
        assert_eq!(on_color("#ffd23d"), "#1a0f12");
    }

    #[test]
    fn a_saved_address_key_is_per_network_and_per_address() {
        let a: IpAddr = "203.0.113.7".parse().unwrap();
        let b: IpAddr = "203.0.113.8".parse().unwrap();
        assert_ne!(trusted_key("public", a), trusted_key("public", b));
        assert_ne!(trusted_key("public", a), trusted_key("private", a));
        assert_ne!(trusted_host("public", a), "app.example.com");
        // The v4-mapped form of an address is the same client.
        let mapped: IpAddr = "::ffff:203.0.113.7".parse().unwrap();
        assert_eq!(trusted_key("public", a), trusted_key("public", mapped));
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(human_duration(Duration::from_secs(30 * 86_400)), "30 days");
        assert_eq!(human_duration(Duration::from_secs(86_400)), "1 day");
        assert_eq!(human_duration(Duration::from_secs(3 * 3_600)), "3 hours");
    }
}
