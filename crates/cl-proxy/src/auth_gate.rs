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
//! `redirect_regex` and static routes never touch a container. It reuses the
//! same `auth.oidc` cluster config and client secret as the web ui's own
//! login (`_clouduler_oidc_client_secret`), so there is one identity
//! provider client, not one per protected app — only its list of allowed
//! redirect URIs in the provider grows, one per protected hostname.
//!
//! Deliberately does not attempt cross-app single sign-on: the cookie is
//! host-only (no `Domain=` attribute), so each protected hostname's session
//! is independent. Simpler and smaller blast radius than a zone-wide cookie,
//! at the cost of a separate login per app.

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
use hyper::{Request, Response, StatusCode};
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use tokio::sync::{Mutex, RwLock};
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

/// How long an in-flight login may sit between redirect and callback.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long a session cookie is valid for once issued.
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
/// Signature label prefix, scoping this HMAC use away from any other signed
/// value the cluster key might back (see `cl_secret::internal_auth`).
const LABEL_PREFIX: &str = "proxyauth";

struct PendingLogin {
    nonce: Nonce,
    pkce_verifier: PkceCodeVerifier,
    return_to: String,
    started_at: Instant,
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
    /// Cached discovery, invalidated whenever the configured issuer changes.
    metadata: RwLock<Option<(String, CoreProviderMetadata)>>,
    pending: Mutex<HashMap<String, PendingLogin>>,
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
            pending: Mutex::new(HashMap::new()),
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
        if let Some((cached_issuer, cached)) = self.metadata.read().await.as_ref() {
            if cached_issuer == issuer {
                return Ok(cached.clone());
            }
        }
        let issuer_url =
            IssuerUrl::new(issuer.to_string()).map_err(|e| format!("bad issuer url: {e}"))?;
        let discovered = CoreProviderMetadata::discover_async(issuer_url, &self.http)
            .await
            .map_err(|e| format!("oidc discovery failed: {e}"))?;
        *self.metadata.write().await = Some((issuer.to_string(), discovered.clone()));
        Ok(discovered)
    }

    async fn gc_expired(&self) {
        let mut pending = self.pending.lock().await;
        pending.retain(|_, login| login.started_at.elapsed() <= LOGIN_TIMEOUT);
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
        if let Some(subject) = self.valid_session(req) {
            let _ = subject;
            return GateOutcome::Allow;
        }
        if path == LOGIN_PATH {
            return self.start_login(req, scheme, &host, &settings).await;
        }
        GateOutcome::Respond(redirect_to_login(scheme, &host, req.uri()))
    }

    fn valid_session(&self, req: &Request<Incoming>) -> Option<String> {
        let key = self.key()?;
        let cookie = extract_cookie(req, COOKIE_NAME)?;
        verify_cookie(&key, &cookie)
    }

    async fn start_login(
        &self,
        req: &Request<Incoming>,
        scheme: &str,
        host: &str,
        settings: &cl_config::OidcConfig,
    ) -> GateOutcome {
        let metadata = match self.metadata(&settings.issuer).await {
            Ok(m) => m,
            Err(e) => return GateOutcome::Respond(bad_gateway(format!("oidc discovery: {e}"))),
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

        let return_to = return_target(req.uri());
        self.gc_expired().await;
        self.pending.lock().await.insert(
            csrf_token.secret().clone(),
            PendingLogin {
                nonce,
                pkce_verifier,
                return_to,
                started_at: Instant::now(),
            },
        );

        GateOutcome::Respond(redirect_response(auth_url.as_str()))
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

        let Some(pending) = self.pending.lock().await.remove(&state) else {
            return GateOutcome::Respond(status_response(
                StatusCode::UNAUTHORIZED,
                "unknown or expired login",
            ));
        };
        if pending.started_at.elapsed() > LOGIN_TIMEOUT {
            return GateOutcome::Respond(status_response(
                StatusCode::UNAUTHORIZED,
                "login took too long",
            ));
        }

        let metadata = match self.metadata(&settings.issuer).await {
            Ok(m) => m,
            Err(e) => return GateOutcome::Respond(bad_gateway(format!("oidc discovery: {e}"))),
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

        let token_response = match client
            .exchange_code(AuthorizationCode::new(code))
            .map_err(|e| format!("token endpoint not configured: {e}"))
        {
            Ok(req) => match req
                .set_pkce_verifier(pending.pkce_verifier)
                .request_async(&self.http)
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    return GateOutcome::Respond(status_response(
                        StatusCode::UNAUTHORIZED,
                        &format!("token exchange failed: {e}"),
                    ))
                }
            },
            Err(e) => return GateOutcome::Respond(bad_gateway(e)),
        };

        let Some(id_token) = token_response.id_token() else {
            return GateOutcome::Respond(status_response(
                StatusCode::UNAUTHORIZED,
                "provider did not return an id token",
            ));
        };
        let verifier = client.id_token_verifier();
        let claims = match id_token.claims(&verifier, &pending.nonce) {
            Ok(c) => c,
            Err(e) => {
                return GateOutcome::Respond(status_response(
                    StatusCode::UNAUTHORIZED,
                    &format!("id token verification failed: {e}"),
                ))
            }
        };
        let subject = claims.subject().as_str().to_string();

        let expires_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + SESSION_TTL.as_secs();
        let cookie_value = sign_cookie(&key, &subject, expires_at);
        let mut resp = redirect_response(&pending.return_to);
        if let Ok(v) = HeaderValue::from_str(&format!(
            "{COOKIE_NAME}={cookie_value}; Path=/; Max-Age={}; Secure; HttpOnly; SameSite=Lax",
            SESSION_TTL.as_secs()
        )) {
            resp.headers_mut().insert(SET_COOKIE, v);
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

fn extract_cookie(req: &Request<Incoming>, name: &str) -> Option<String> {
    let raw = req.headers().get(COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|kv| {
        let kv = kv.trim();
        let (k, v) = kv.split_once('=')?;
        (k == name).then(|| v.to_string())
    })
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
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let decode = |s: &str| -> String {
                let bytes = s.as_bytes();
                let mut out = Vec::with_capacity(bytes.len());
                let mut i = 0;
                while i < bytes.len() {
                    match bytes[i] {
                        b'+' => {
                            out.push(b' ');
                            i += 1;
                        }
                        b'%' if i + 2 < bytes.len() => {
                            if let Ok(byte) = u8::from_str_radix(
                                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                                16,
                            ) {
                                out.push(byte);
                                i += 3;
                            } else {
                                out.push(bytes[i]);
                                i += 1;
                            }
                        }
                        b => {
                            out.push(b);
                            i += 1;
                        }
                    }
                }
                String::from_utf8_lossy(&out).into_owned()
            };
            Some((decode(k), decode(v)))
        })
        .collect()
}

/// Signs `subject` and `expires_at` (unix seconds) into a cookie value that
/// any node holding `key` can verify without asking anyone else, since the
/// signature is the only state the session carries.
fn sign_cookie(key: &ClusterKey, subject: &str, expires_at: u64) -> String {
    let label = format!("{LABEL_PREFIX}:{subject}:{expires_at}");
    let sig = internal_auth::sign(key, &label);
    let subject_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(subject.as_bytes());
    format!("{subject_b64}.{expires_at}.{sig}")
}

/// Verifies a cookie value produced by `sign_cookie`, returning the subject
/// if the signature checks out and it has not expired.
fn verify_cookie(key: &ClusterKey, value: &str) -> Option<String> {
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
    let label = format!("{LABEL_PREFIX}:{subject}:{expires_at}");
    internal_auth::verify(key, &label, sig).then_some(subject)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cookie_verifies_against_the_key_that_signed_it() {
        let key = ClusterKey::generate();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cookie = sign_cookie(&key, "alice@example.com", now + 3600);
        assert_eq!(
            verify_cookie(&key, &cookie),
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
        let cookie = sign_cookie(&key, "alice@example.com", now.saturating_sub(1));
        assert_eq!(verify_cookie(&key, &cookie), None);
    }

    #[test]
    fn a_cookie_signed_by_a_different_key_does_not_verify() {
        let key = ClusterKey::generate();
        let other = ClusterKey::generate();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cookie = sign_cookie(&key, "alice@example.com", now + 3600);
        assert_eq!(verify_cookie(&other, &cookie), None);
    }

    #[test]
    fn a_tampered_subject_does_not_verify() {
        let key = ClusterKey::generate();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cookie = sign_cookie(&key, "alice@example.com", now + 3600);
        let (_, rest) = cookie.split_once('.').unwrap();
        let forged = format!(
            "{}.{rest}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"mallory@example.com")
        );
        assert_eq!(verify_cookie(&key, &forged), None);
    }

    #[test]
    fn garbage_cookie_values_do_not_panic_or_verify() {
        let key = ClusterKey::generate();
        assert_eq!(verify_cookie(&key, "not-a-cookie"), None);
        assert_eq!(verify_cookie(&key, ""), None);
        assert_eq!(verify_cookie(&key, "a.b.c"), None);
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
}
