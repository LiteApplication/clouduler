//! Authorization code flow with PKCE.
//!
//! `CoreClient` fixes `EmptyAdditionalClaims`, but the admin role check needs an arbitrary claim
//! (`admin_claim`, often `groups`). Rather than plumb a custom claims type through the generic
//! `Client` alias, the id token is verified by openidconnect as usual and its already-verified
//! payload re-read as loose json for that one claim; no second source of trust.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cl_raft::ClusterNode;
use cl_secret::{ClusterKey, Sealed};
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use tokio::sync::{Mutex, OnceCell, RwLock};
use tracing::warn;

use crate::error::ApiError;
use crate::oidc::OIDC_CLIENT_SECRET;

use super::{Identity, Role};

pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// `/auth/oidc/login` is unauthenticated, so without a cap a flood grows `pending` without bound.
const MAX_PENDING_LOGINS: usize = 1024;
/// Ties a login's `state` to the browser that started it; without it an attacker could start a
/// login with their own account and hand the callback url to a victim.
pub const STATE_COOKIE_NAME: &str = "cl_oidc_state";
pub const STATE_COOKIE_PATH: &str = "/auth/oidc/callback";

pub struct OidcSettings {
    pub issuer: String,
    pub client_id: String,
    pub redirect_url: String,
    pub scopes: Vec<String>,
    pub admin_claim: String,
    pub admin_value: Option<String>,
    pub username_claim: String,
}

struct PendingLogin {
    nonce: Nonce,
    pkce_verifier: PkceCodeVerifier,
    started_at: Instant,
}

#[derive(Default)]
struct PendingLogins {
    logins: Mutex<HashMap<String, PendingLogin>>,
}

impl PendingLogins {
    /// Rejects with 429 when full of unexpired entries rather than evicting the oldest: eviction
    /// would let a flood push real users' logins out. Expired entries are only swept at the cap,
    /// keeping the common path O(1).
    async fn insert(&self, state: String, login: PendingLogin) -> Result<(), ApiError> {
        let mut logins = self.logins.lock().await;
        if logins.len() >= MAX_PENDING_LOGINS {
            logins.retain(|_, login| login.started_at.elapsed() <= LOGIN_TIMEOUT);
            if logins.len() >= MAX_PENDING_LOGINS {
                return Err(ApiError::TooManyRequests(
                    "too many logins in progress, try again shortly".into(),
                ));
            }
        }
        logins.insert(state, login);
        Ok(())
    }

    async fn take(&self, state: &str) -> Option<PendingLogin> {
        self.logins.lock().await.remove(state)
    }

    async fn prune_expired(&self) {
        self.logins
            .lock()
            .await
            .retain(|_, login| login.started_at.elapsed() <= LOGIN_TIMEOUT);
    }
}

pub struct OidcAuthenticator {
    settings: OidcSettings,
    http: openidconnect::reqwest::Client,
    /// Discovery is cached; the client is rebuilt per call because `from_provider_metadata` returns a
    /// type that is not nameable as the bare `CoreClient` alias. Rebuilding does no IO.
    metadata: OnceCell<CoreProviderMetadata>,
    pending: PendingLogins,
    /// The client secret is a sealed cluster secret opened on every login, so it can be set or
    /// rotated from the web ui without a restart. See `crate::routes::oidc`.
    cluster: Arc<ClusterNode>,
    key: Arc<RwLock<Option<Arc<ClusterKey>>>>,
}

impl OidcAuthenticator {
    pub fn new(
        settings: OidcSettings,
        cluster: Arc<ClusterNode>,
        key: Arc<RwLock<Option<Arc<ClusterKey>>>>,
    ) -> Result<Self, ApiError> {
        let http = openidconnect::reqwest::ClientBuilder::new()
            // Discovery and token requests go to a fixed issuer; following a redirect would open an SSRF path.
            .redirect(openidconnect::reqwest::redirect::Policy::none())
            .build()
            .map_err(ApiError::internal)?;
        Ok(Self {
            settings,
            http,
            metadata: OnceCell::new(),
            pending: PendingLogins::default(),
            cluster,
            key,
        })
    }

    /// `None` when there is no key, no secret or it cannot be opened; a provider requiring a secret
    /// then fails authentication rather than trying with none.
    async fn client_secret(&self) -> Option<String> {
        let key_guard = self.key.read().await;
        let key = key_guard.as_ref()?;
        let state = self.cluster.snapshot().await;
        let entry = state.secrets.get(OIDC_CLIENT_SECRET)?;
        match Sealed::from_blob(entry.sealed.clone()).open(key, OIDC_CLIENT_SECRET) {
            Ok(plaintext) => match String::from_utf8(plaintext) {
                Ok(secret) => Some(secret),
                Err(_) => {
                    warn!("the oidc client secret is not utf8");
                    None
                }
            },
            Err(e) => {
                warn!(error = %e, "cannot open the oidc client secret");
                None
            }
        }
    }

    /// Discover once and cache. Building the `CoreClient` is left to each caller: the type returned by
    /// `from_provider_metadata(..).set_redirect_uri(..)` is not nameable as the bare alias.
    async fn metadata(&self) -> Result<&CoreProviderMetadata, ApiError> {
        self.metadata
            .get_or_try_init(|| async {
                let issuer = IssuerUrl::new(self.settings.issuer.clone())
                    .map_err(|e| ApiError::Internal(format!("bad issuer url: {e}")))?;
                CoreProviderMetadata::discover_async(issuer, &self.http)
                    .await
                    .map_err(|e| ApiError::Internal(format!("oidc discovery failed: {e}")))
            })
            .await
    }

    /// Returns the redirect url and the `state`, which the caller must also bind to the browser in a cookie.
    pub async fn start_login(&self) -> Result<(String, String), ApiError> {
        let metadata = self.metadata().await?;
        let redirect = RedirectUrl::new(self.settings.redirect_url.clone())
            .map_err(|e| ApiError::Internal(format!("bad redirect url: {e}")))?;
        let client = CoreClient::from_provider_metadata(
            metadata.clone(),
            ClientId::new(self.settings.client_id.clone()),
            self.client_secret().await.map(ClientSecret::new),
        )
        .set_redirect_uri(redirect);
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

        let mut request = client.authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        );
        for scope in &self.settings.scopes {
            request = request.add_scope(Scope::new(scope.clone()));
        }
        let (auth_url, csrf_token, nonce) = request.set_pkce_challenge(pkce_challenge).url();

        let state = csrf_token.secret().clone();
        self.pending
            .insert(
                state.clone(),
                PendingLogin {
                    nonce,
                    pkce_verifier,
                    started_at: Instant::now(),
                },
            )
            .await?;

        Ok((auth_url.to_string(), state))
    }

    pub async fn finish_login(&self, code: String, state: String) -> Result<Identity, ApiError> {
        let pending =
            self.pending.take(&state).await.ok_or_else(|| {
                ApiError::Unauthenticated("unknown or expired login state".into())
            })?;
        if pending.started_at.elapsed() > LOGIN_TIMEOUT {
            return Err(ApiError::Unauthenticated("login took too long".into()));
        }

        let metadata = self.metadata().await?;
        let redirect = RedirectUrl::new(self.settings.redirect_url.clone())
            .map_err(|e| ApiError::Internal(format!("bad redirect url: {e}")))?;
        let client = CoreClient::from_provider_metadata(
            metadata.clone(),
            ClientId::new(self.settings.client_id.clone()),
            self.client_secret().await.map(ClientSecret::new),
        )
        .set_redirect_uri(redirect);
        let token_response = client
            .exchange_code(AuthorizationCode::new(code))
            .map_err(|e| ApiError::Internal(format!("token endpoint not configured: {e}")))?
            .set_pkce_verifier(pending.pkce_verifier)
            .request_async(&self.http)
            .await
            .map_err(|e| ApiError::Unauthenticated(format!("token exchange failed: {e}")))?;

        let id_token = token_response.id_token().ok_or_else(|| {
            ApiError::Unauthenticated("provider did not return an id token".into())
        })?;
        let verifier = client.id_token_verifier();
        let claims = id_token
            .claims(&verifier, &pending.nonce)
            .map_err(|e| ApiError::Unauthenticated(format!("id token verification failed: {e}")))?;

        let subject = claims.subject().as_str().to_string();
        let role = self.role_from_raw_claims(id_token);
        let username = self
            .username_from_raw_claims(id_token)
            .unwrap_or_else(|| subject.clone());

        Ok(Identity {
            subject,
            username,
            role,
        })
    }

    /// Safe after `claims()` has verified the token; see the module doc.
    fn role_from_raw_claims(&self, id_token: &openidconnect::core::CoreIdToken) -> Role {
        let Some(admin_value) = &self.settings.admin_value else {
            return Role::Viewer;
        };
        let Some(payload) = decode_jwt_payload(&id_token.to_string()) else {
            return Role::Viewer;
        };
        let Some(claim) = payload.get(&self.settings.admin_claim) else {
            return Role::Viewer;
        };
        let matches = match claim {
            serde_json::Value::String(s) => s == admin_value,
            serde_json::Value::Array(items) => items
                .iter()
                .any(|v| v.as_str().is_some_and(|s| s == admin_value)),
            _ => false,
        };
        if matches {
            Role::Admin
        } else {
            Role::Viewer
        }
    }

    /// `None` when absent or not a string, so the caller falls back to the subject.
    fn username_from_raw_claims(
        &self,
        id_token: &openidconnect::core::CoreIdToken,
    ) -> Option<String> {
        let payload = decode_jwt_payload(&id_token.to_string())?;
        payload
            .get(&self.settings.username_claim)?
            .as_str()
            .map(str::to_string)
    }

    pub async fn prune_expired(&self) {
        self.pending.prune_expired().await;
    }
}

/// Runs before any token exchange: a missing or different cookie means this browser did not start the login.
pub fn check_state_cookie(cookie: Option<&str>, state: &str) -> Result<(), ApiError> {
    match cookie {
        Some(cookie) if constant_time_eq(cookie.as_bytes(), state.as_bytes()) => Ok(()),
        _ => Err(ApiError::Unauthenticated(
            "login state does not match this browser, start the login again".into(),
        )),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn decode_jwt_payload(compact: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    use base64::Engine;
    let payload = compact.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice::<serde_json::Value>(&raw)
        .ok()?
        .as_object()
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jwt(payload_json: &str) -> String {
        use base64::Engine;
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload_json);
        format!("{header}.{payload}.sig")
    }

    fn login(age: Duration) -> PendingLogin {
        PendingLogin {
            nonce: Nonce::new("n".into()),
            pkce_verifier: PkceCodeVerifier::new("v".into()),
            started_at: Instant::now().checked_sub(age).expect("monotonic clock"),
        }
    }

    #[tokio::test]
    async fn a_full_pending_map_rejects_with_429() {
        let pending = PendingLogins::default();
        for i in 0..MAX_PENDING_LOGINS {
            pending
                .insert(format!("s{i}"), login(Duration::ZERO))
                .await
                .expect("room left");
        }
        let err = pending
            .insert("extra".into(), login(Duration::ZERO))
            .await
            .expect_err("full");
        assert!(matches!(err, ApiError::TooManyRequests(_)));
        use axum::response::IntoResponse;
        assert_eq!(
            err.into_response().status(),
            axum::http::StatusCode::TOO_MANY_REQUESTS
        );
        assert!(pending.take("s0").await.is_some());
    }

    #[tokio::test]
    async fn a_full_map_of_expired_logins_makes_room() {
        let pending = PendingLogins::default();
        for i in 0..MAX_PENDING_LOGINS {
            pending
                .insert(format!("s{i}"), login(LOGIN_TIMEOUT * 2))
                .await
                .unwrap();
        }
        pending
            .insert("fresh".into(), login(Duration::ZERO))
            .await
            .expect("expired entries are swept at the cap");
        assert!(pending.take("fresh").await.is_some());
    }

    #[tokio::test]
    async fn pruning_drops_only_expired_logins() {
        let pending = PendingLogins::default();
        pending
            .insert("old".into(), login(LOGIN_TIMEOUT * 2))
            .await
            .unwrap();
        pending
            .insert("new".into(), login(Duration::ZERO))
            .await
            .unwrap();
        pending.prune_expired().await;
        assert!(pending.take("old").await.is_none());
        assert!(pending.take("new").await.is_some());
    }

    #[test]
    fn the_state_cookie_must_be_present_and_equal() {
        assert!(check_state_cookie(Some("abc"), "abc").is_ok());
        assert!(check_state_cookie(None, "abc").is_err());
        assert!(check_state_cookie(Some("abd"), "abc").is_err());
        assert!(check_state_cookie(Some("ab"), "abc").is_err());
    }

    #[test]
    fn decodes_the_middle_segment_of_a_jwt() {
        let jwt = make_jwt(r#"{"groups":["admins","dev"]}"#);
        let payload = decode_jwt_payload(&jwt).expect("decodes");
        assert!(payload.contains_key("groups"));
    }

    #[test]
    fn malformed_tokens_decode_to_nothing() {
        assert!(decode_jwt_payload("not-a-jwt").is_none());
        assert!(decode_jwt_payload("a.b").is_some() || decode_jwt_payload("a.b").is_none());
    }
}
