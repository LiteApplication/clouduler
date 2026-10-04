//! `authenticated` middleware: gate a route behind clouduler's own OIDC
//! login, independent of `forward_auth`.
//!
//! Every node runs its own proxy listener with no shared session store (see
//! `server.rs`'s module doc: routes are reachable from every node over the
//! overlay), so a session cannot be an opaque id looked up in a per-node map
//! the way the web ui's login works — a browser can land on any node for a
//! route published on every node. Instead the session cookie carries its own
//! HMAC signature under the cluster key (`cl_secret::internal_auth`), so any
//! node holding the key verifies it without asking anyone else or sharing
//! state.
//!
//! The OIDC dance itself (redirect to the provider, exchange the code) is
//! synthesized here rather than forwarded to a backend, the same way
//! `redirect_regex` and static routes never touch a container. The in-flight
//! login state (csrf state, nonce, pkce verifier, return target) is likewise
//! kept out of any per-node map: it travels in a short-lived `cl_proxy_login`
//! cookie sealed under the cluster key, so the callback can land on a
//! different node than the redirect did, and the state is bound to the browser
//! that started the login (a forged callback link carries no matching cookie).
//! It reuses the
//! same `auth.oidc` cluster config and client secret as the web ui's own
//! login (`_clouduler_oidc_client_secret`), so there is one identity
//! provider client, not one per protected app — only its list of allowed
//! redirect URIs in the provider grows, one per protected hostname.
//!
//! Deliberately does not attempt cross-app single sign-on: the cookie is
//! host-only (no `Domain=` attribute), so each protected hostname's session
//! is independent. Simpler and smaller blast radius than a zone-wide cookie,
//! at the cost of a separate login per app. The signed session label includes
//! the host, so a cookie minted for one app does not verify on another, and
//! the proxy strips its own cookies from requests before forwarding them so a
//! backend never sees a replayable session.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use cl_config::ClusterConfig;
use cl_raft::node::ClusterNode;
use cl_secret::{internal_auth, ClusterKey, Sealed};
use http::{HeaderValue, Uri};
use hyper::body::Incoming;
use hyper::header::{COOKIE, HOST, LOCATION, SET_COOKIE};
use hyper::HeaderMap;
use hyper::{Request, Response, StatusCode};
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::warn;

use crate::server::{empty_body, text_body, ProxyBody};

/// Reserved name for the sealed cluster secret holding the OIDC client
/// secret. Same value as `cl_api::oidc::OIDC_CLIENT_SECRET` deliberately: one
/// Authentik (or any OIDC provider) client backs both the web ui and every
/// `authenticated` route, so there is exactly one secret to configure.
const OIDC_CLIENT_SECRET: &str = "_clouduler_oidc_client_secret";

/// Path this middleware answers itself rather than proxying, starting a
/// login. Under the route's own hostname, so no extra dns or certificate is
/// needed beyond what the protected route already has.
pub const LOGIN_PATH: &str = "/.clouduler/oidc/login";
/// Where the provider redirects back to after the user authenticates.
pub const CALLBACK_PATH: &str = "/.clouduler/oidc/callback";
/// Cookie carrying the signed session.
pub const COOKIE_NAME: &str = "cl_proxy_auth";
/// Cookie carrying the sealed in-flight login state between the redirect to
/// the provider and its callback.
pub const LOGIN_COOKIE_NAME: &str = "cl_proxy_login";
/// Path scope of the login cookie: only the callback needs it.
const LOGIN_COOKIE_PATH: &str = "/.clouduler/oidc/";
/// Context (aad) the login cookie is sealed under, so a sealed value of any
/// other kind cannot be replayed as a login cookie.
const LOGIN_SEAL_CONTEXT: &str = "proxy-login";
/// How long cached provider discovery (including its JWKS) is trusted before
/// being refetched, so a rotated signing key does not break logins forever.
const METADATA_TTL: Duration = Duration::from_secs(3600);

/// How long an in-flight login may sit between redirect and callback.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long a session cookie is valid for once issued.
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
/// Signature label prefix, scoping this HMAC use away from any other signed
/// value the cluster key might back (see `cl_secret::internal_auth`).
const LABEL_PREFIX: &str = "proxyauth";

/// In-flight login state, sealed into the `cl_proxy_login` cookie.
#[derive(Serialize, Deserialize)]
struct PendingLogin {
    state: String,
    nonce: String,
    pkce_verifier: String,
    return_to: String,
    started_at_unix: u64,
}

/// What a request should get in response to the `authenticated` middleware.
pub enum GateOutcome {
    /// The session is valid; proceed to the backend.
    Allow,
    /// Answer directly: a login/callback step, a redirect to start one, or a
    /// hard failure (e.g. no oidc configured at all).
    Respond(Response<ProxyBody>),
}

/// Runtime state for the `authenticated` middleware, shared across every
/// route that uses it. Reads config and the cluster key live on each
/// request rather than needing a rebuild on change, since both can change
/// independently of the proxy's listeners.
pub struct AuthGate {
    config_rx: tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
    key_rx: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
    cluster: Arc<ClusterNode>,
    http: openidconnect::reqwest::Client,
    /// Cached discovery, invalidated whenever the configured issuer changes
    /// or the entry is older than `METADATA_TTL`.
    metadata: RwLock<Option<(String, Instant, CoreProviderMetadata)>>,
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

    fn settings(&self) -> Option<cl_config::OidcConfig> {
        self.config_rx.borrow().auth.oidc.clone()
    }

    fn key(&self) -> Option<Arc<ClusterKey>> {
        self.key_rx.borrow().clone()
    }

    async fn client_secret(&self) -> Option<String> {
        let key = self.key()?;
        let state = self.cluster.state().await;
        let entry = state.secrets.get(OIDC_CLIENT_SECRET)?;
        match Sealed::from_blob(entry.sealed.clone()).open(&key, OIDC_CLIENT_SECRET) {
            Ok(plaintext) => String::from_utf8(plaintext).ok(),
            Err(e) => {
                warn!(error = %e, "cannot open the oidc client secret for the authenticated middleware");
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

    /// Fetch discovery unconditionally and cache it.
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

    /// Decide what a request to an `authenticated` route should get: the
    /// login start, the provider's callback, a valid session let through, or
    /// a redirect to start a new login.
    pub async fn evaluate(&self, req: &Request<Incoming>, scheme: &str) -> GateOutcome {
        let Some(settings) = self.settings() else {
            return GateOutcome::Respond(status_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "this route requires login, but the cluster has no auth.oidc configured",
            ));
        };
        let Some(host) = req.headers().get(HOST).and_then(|v| v.to_str().ok()) else {
            return GateOutcome::Respond(status_response(
                StatusCode::BAD_REQUEST,
                "missing host header",
            ));
        };
        let host = host.to_string();
        let path = req.uri().path();

        if path == CALLBACK_PATH {
            return self.finish_login(req, scheme, &host, &settings).await;
        }
        if self.valid_session(req, &host).is_some() {
            return GateOutcome::Allow;
        }
        if path == LOGIN_PATH {
            return self.start_login(req, scheme, &host, &settings).await;
        }
        GateOutcome::Respond(redirect_to_login(scheme, &host, req.uri()))
    }

    fn valid_session(&self, req: &Request<Incoming>, host: &str) -> Option<String> {
        let key = self.key()?;
        let cookie = extract_cookie(req.headers(), COOKIE_NAME)?;
        verify_cookie(&key, host, &cookie)
    }

    async fn start_login(
        &self,
        req: &Request<Incoming>,
        scheme: &str,
        host: &str,
        settings: &cl_config::OidcConfig,
    ) -> GateOutcome {
        let Some(key) = self.key() else {
            return GateOutcome::Respond(status_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "this node holds no cluster key",
            ));
        };
        let metadata = match self.metadata(&settings.issuer).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "oidc discovery failed while starting a login");
                return GateOutcome::Respond(bad_gateway("oidc discovery failed"));
            }
        };
        let redirect_url = format!("{scheme}://{host}{CALLBACK_PATH}");
        let Ok(redirect) = RedirectUrl::new(redirect_url) else {
            return GateOutcome::Respond(bad_gateway("invalid redirect url"));
        };
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(settings.client_id.clone()),
            self.client_secret().await.map(ClientSecret::new),
        )
        .set_redirect_uri(redirect);

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

        let return_to = login_return_param(req.uri());
        let pending = PendingLogin {
            state: csrf_token.secret().clone(),
            nonce: nonce.secret().clone(),
            pkce_verifier: pkce_verifier.secret().clone(),
            return_to,
            started_at_unix: now_unix(),
        };
        let Some(sealed) = seal_login(&key, &pending) else {
            return GateOutcome::Respond(bad_gateway("cannot seal login state"));
        };

        let mut resp = redirect_response(auth_url.as_str());
        if let Ok(v) = HeaderValue::from_str(&format!(
            "{LOGIN_COOKIE_NAME}={sealed}; Path={LOGIN_COOKIE_PATH}; Max-Age={}; Secure; HttpOnly; SameSite=Lax",
            LOGIN_TIMEOUT.as_secs()
        )) {
            resp.headers_mut().append(SET_COOKIE, v);
        }
        GateOutcome::Respond(resp)
    }

    async fn finish_login(
        &self,
        req: &Request<Incoming>,
        scheme: &str,
        host: &str,
        settings: &cl_config::OidcConfig,
    ) -> GateOutcome {
        let Some(key) = self.key() else {
            return GateOutcome::Respond(status_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "this node holds no cluster key",
            ));
        };
        let query = req.uri().query().unwrap_or("");
        let params: HashMap<String, String> = form_urlencoded_parse(query);
        let (Some(code), Some(state)) = (params.get("code").cloned(), params.get("state").cloned())
        else {
            return GateOutcome::Respond(status_response(
                StatusCode::BAD_REQUEST,
                "missing code or state",
            ));
        };

        let Some(pending) =
            extract_cookie(req.headers(), LOGIN_COOKIE_NAME).and_then(|v| open_login(&key, &v))
        else {
            return GateOutcome::Respond(status_response(
                StatusCode::UNAUTHORIZED,
                "unknown or expired login",
            ));
        };
        if !constant_time_eq(pending.state.as_bytes(), state.as_bytes()) {
            return GateOutcome::Respond(status_response(
                StatusCode::UNAUTHORIZED,
                "unknown or expired login",
            ));
        }
        if now_unix().saturating_sub(pending.started_at_unix) > LOGIN_TIMEOUT.as_secs() {
            return GateOutcome::Respond(status_response(
                StatusCode::UNAUTHORIZED,
                "login took too long",
            ));
        }

        let metadata = match self.metadata(&settings.issuer).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "oidc discovery failed while finishing a login");
                return GateOutcome::Respond(bad_gateway("oidc discovery failed"));
            }
        };
        let redirect_url = format!("{scheme}://{host}{CALLBACK_PATH}");
        let Ok(redirect) = RedirectUrl::new(redirect_url) else {
            return GateOutcome::Respond(bad_gateway("invalid redirect url"));
        };
        let client_secret = self.client_secret().await;
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(settings.client_id.clone()),
            client_secret.clone().map(ClientSecret::new),
        )
        .set_redirect_uri(redirect.clone());

        let token_request = match client.exchange_code(AuthorizationCode::new(code)) {
            Ok(req) => req,
            Err(e) => {
                warn!(error = %e, "oidc token endpoint not configured");
                return GateOutcome::Respond(bad_gateway("token endpoint not configured"));
            }
        };
        let token_response = match token_request
            .set_pkce_verifier(PkceCodeVerifier::new(pending.pkce_verifier.clone()))
            .request_async(&self.http)
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                warn!(error = %e, "oidc token exchange failed");
                return GateOutcome::Respond(status_response(
                    StatusCode::UNAUTHORIZED,
                    "token exchange failed",
                ));
            }
        };

        let Some(id_token) = token_response.id_token() else {
            return GateOutcome::Respond(status_response(
                StatusCode::UNAUTHORIZED,
                "provider did not return an id token",
            ));
        };
        let nonce = Nonce::new(pending.nonce.clone());
        let subject = match id_token
            .claims(&client.id_token_verifier(), &nonce)
            .map(|c| c.subject().as_str().to_string())
        {
            Ok(s) => s,
            Err(first) => {
                // A rotated signing key looks exactly like a bad signature
                // against the cached jwks, so refetch discovery once and
                // retry before giving up.
                let retried = match self.refresh_metadata(&settings.issuer).await {
                    Ok(fresh) => {
                        let client = CoreClient::from_provider_metadata(
                            fresh,
                            ClientId::new(settings.client_id.clone()),
                            client_secret.map(ClientSecret::new),
                        )
                        .set_redirect_uri(redirect);
                        let verifier = client.id_token_verifier();
                        let result = id_token
                            .claims(&verifier, &nonce)
                            .map(|c| c.subject().as_str().to_string())
                            .map_err(|e| e.to_string());
                        result
                    }
                    Err(e) => Err(e),
                };
                match retried {
                    Ok(s) => s,
                    Err(second) => {
                        warn!(first = %first, second = %second, "oidc id token verification failed");
                        return GateOutcome::Respond(status_response(
                            StatusCode::UNAUTHORIZED,
                            "id token verification failed",
                        ));
                    }
                }
            }
        };

        let expires_at = now_unix() + SESSION_TTL.as_secs();
        let cookie_value = sign_cookie(&key, host, &subject, expires_at);
        let mut resp = redirect_response(&pending.return_to);
        if let Ok(v) = HeaderValue::from_str(&format!(
            "{COOKIE_NAME}={cookie_value}; Path=/; Max-Age={}; Secure; HttpOnly; SameSite=Lax",
            SESSION_TTL.as_secs()
        )) {
            resp.headers_mut().append(SET_COOKIE, v);
        }
        // The login state is single-use; drop it so the browser does not keep
        // a sealed copy around until it times out.
        if let Ok(v) = HeaderValue::from_str(&format!(
            "{LOGIN_COOKIE_NAME}=; Path={LOGIN_COOKIE_PATH}; Max-Age=0; Secure; HttpOnly; SameSite=Lax"
        )) {
            resp.headers_mut().append(SET_COOKIE, v);
        }
        GateOutcome::Respond(resp)
    }
}

/// Where to send the browser back to once login finishes: the path and
/// query the user actually asked for, or `/` if that was the login path
/// itself (a user hitting `/.clouduler/oidc/login` directly).
fn return_target(uri: &Uri) -> String {
    let candidate = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if candidate.starts_with(LOGIN_PATH) {
        "/".to_string()
    } else {
        candidate.to_string()
    }
}

/// The post-login target carried in the login request's `return` query
/// parameter, or `/` when it is absent or not a safe same-site path.
fn login_return_param(uri: &Uri) -> String {
    let params = form_urlencoded_parse(uri.query().unwrap_or(""));
    match params.get("return") {
        Some(r) if is_safe_return(r) => r.clone(),
        _ => "/".to_string(),
    }
}

/// Accepts only a single-slash absolute path, since the value ends up in a
/// `Location` header and anything else (`//evil`, `/\evil`, header
/// injection, our own reserved paths) is an open redirect or a loop.
fn is_safe_return(r: &str) -> bool {
    r.starts_with('/')
        && !r.starts_with("//")
        && !r.starts_with("/\\")
        && !r.contains(['\\', '\r', '\n'])
        && !r.starts_with("/.clouduler/")
}

fn redirect_to_login(scheme: &str, host: &str, original: &Uri) -> Response<ProxyBody> {
    let return_to = return_target(original);
    let encoded = urlencode(&return_to);
    redirect_response(&format!("{scheme}://{host}{LOGIN_PATH}?return={encoded}"))
}

fn redirect_response(location: &str) -> Response<ProxyBody> {
    Response::builder()
        .status(StatusCode::FOUND)
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
            name != COOKIE_NAME && name != LOGIN_COOKIE_NAME
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

/// Lowercases the host and strips any port, so the signed label is the same
/// however the browser spelled the authority.
fn normalize_host(host: &str) -> String {
    let host = host.trim().to_ascii_lowercase();
    if let Some(rest) = host.strip_prefix('[') {
        // Bracketed ipv6 literal: keep everything up to the closing bracket.
        return rest.split(']').next().unwrap_or(rest).to_string();
    }
    host.split(':').next().unwrap_or("").to_string()
}

/// Signs `host`, `subject` and `expires_at` (unix seconds) into a cookie
/// value that any node holding `key` can verify without asking anyone else,
/// since the signature is the only state the session carries. The host is in
/// the signed label so a cookie minted for one app is useless on another.
fn sign_cookie(key: &ClusterKey, host: &str, subject: &str, expires_at: u64) -> String {
    let host = normalize_host(host);
    let label = format!("{LABEL_PREFIX}:{host}:{subject}:{expires_at}");
    let sig = internal_auth::sign(key, &label);
    let subject_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(subject.as_bytes());
    format!("{subject_b64}.{expires_at}.{sig}")
}

/// Verifies a cookie value produced by `sign_cookie` for `host`, returning the
/// subject if the signature checks out and it has not expired.
fn verify_cookie(key: &ClusterKey, host: &str, value: &str) -> Option<String> {
    let mut parts = value.splitn(3, '.');
    let subject_b64 = parts.next()?;
    let expires_at: u64 = parts.next()?.parse().ok()?;
    let sig = parts.next()?;

    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now >= expires_at {
        return None;
    }

    let subject_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(subject_b64)
        .ok()?;
    let subject = String::from_utf8(subject_bytes).ok()?;
    let label = format!(
        "{LABEL_PREFIX}:{}:{subject}:{expires_at}",
        normalize_host(host)
    );
    internal_auth::verify(key, &label, sig).then_some(subject)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_A: &str = "a.example.com";

    #[test]
    fn a_cookie_verifies_against_the_key_that_signed_it() {
        let key = ClusterKey::generate();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cookie = sign_cookie(&key, HOST_A, "alice@example.com", now + 3600);
        assert_eq!(
            verify_cookie(&key, HOST_A, &cookie),
            Some("alice@example.com".to_string())
        );
    }

    #[test]
    fn an_expired_cookie_does_not_verify() {
        let key = ClusterKey::generate();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cookie = sign_cookie(&key, HOST_A, "alice@example.com", now.saturating_sub(1));
        assert_eq!(verify_cookie(&key, HOST_A, &cookie), None);
    }

    #[test]
    fn a_cookie_signed_by_a_different_key_does_not_verify() {
        let key = ClusterKey::generate();
        let other = ClusterKey::generate();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cookie = sign_cookie(&key, HOST_A, "alice@example.com", now + 3600);
        assert_eq!(verify_cookie(&other, HOST_A, &cookie), None);
    }

    #[test]
    fn a_tampered_subject_does_not_verify() {
        let key = ClusterKey::generate();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cookie = sign_cookie(&key, HOST_A, "alice@example.com", now + 3600);
        let (_, rest) = cookie.split_once('.').unwrap();
        let forged = format!(
            "{}.{rest}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"mallory@example.com")
        );
        assert_eq!(verify_cookie(&key, HOST_A, &forged), None);
    }

    #[test]
    fn garbage_cookie_values_do_not_panic_or_verify() {
        let key = ClusterKey::generate();
        assert_eq!(verify_cookie(&key, HOST_A, "not-a-cookie"), None);
        assert_eq!(verify_cookie(&key, HOST_A, ""), None);
        assert_eq!(verify_cookie(&key, HOST_A, "a.b.c"), None);
    }

    #[test]
    fn return_target_defaults_to_root_for_the_login_path_itself() {
        let uri: Uri = "/.clouduler/oidc/login?return=%2Ffoo".parse().unwrap();
        assert_eq!(return_target(&uri), "/");
    }

    #[test]
    fn return_target_keeps_an_ordinary_path() {
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
    fn form_urlencoded_parse_decodes_a_trailing_percent_escape() {
        let params = form_urlencoded_parse("a=x%2F&b=%41");
        assert_eq!(params.get("a").map(String::as_str), Some("x/"));
        assert_eq!(params.get("b").map(String::as_str), Some("A"));
    }

    #[test]
    fn a_cookie_does_not_verify_on_another_host() {
        let key = ClusterKey::generate();
        let cookie = sign_cookie(&key, HOST_A, "alice", now_unix() + 3600);
        assert_eq!(verify_cookie(&key, "b.example.com", &cookie), None);
    }

    #[test]
    fn host_case_and_port_do_not_matter() {
        let key = ClusterKey::generate();
        let cookie = sign_cookie(&key, "A.Example.com:8443", "alice", now_unix() + 3600);
        assert_eq!(
            verify_cookie(&key, "a.example.com", &cookie),
            Some("alice".to_string())
        );
    }

    #[test]
    fn login_state_round_trips_and_is_cookie_safe() {
        let key = ClusterKey::generate();
        let pending = PendingLogin {
            state: "s".into(),
            nonce: "n".into(),
            pkce_verifier: "v".into(),
            return_to: "/x?y=1".into(),
            started_at_unix: 5,
        };
        let sealed = seal_login(&key, &pending).unwrap();
        assert!(sealed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        let back = open_login(&key, &sealed).unwrap();
        assert_eq!(back.state, "s");
        assert_eq!(back.return_to, "/x?y=1");
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
    fn login_return_param_accepts_local_paths_only() {
        let ok = |q: &str| {
            login_return_param(
                &format!("/.clouduler/oidc/login?return={q}")
                    .parse()
                    .unwrap(),
            )
        };
        assert_eq!(ok("%2Flibrary%3Fx%3D1"), "/library?x=1");
        assert_eq!(ok("%2F%2Fevil.com"), "/");
        assert_eq!(ok("%2F%5Cevil.com"), "/");
        assert_eq!(ok("https%3A%2F%2Fevil.com"), "/");
        assert_eq!(ok("%2Fa%5Cb"), "/");
        assert_eq!(ok("%2Fa%0D%0Ab"), "/");
        assert_eq!(ok("%2F.clouduler%2Foidc%2Fcallback"), "/");
        assert_eq!(ok("evil"), "/");
        let none: Uri = "/.clouduler/oidc/login".parse().unwrap();
        assert_eq!(login_return_param(&none), "/");
    }

    #[test]
    fn strip_gate_cookies_keeps_other_cookies() {
        let mut h = HeaderMap::new();
        h.insert(
            COOKIE,
            HeaderValue::from_static("a=1; cl_proxy_auth=zzz; cl_proxy_login=q; b=2"),
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
}
