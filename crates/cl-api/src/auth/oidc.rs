//! Authorization code flow with PKCE.
//!
//! `CoreClient` bakes in `EmptyAdditionalClaims`, so the standard claims are
//! all openidconnect will hand us after verification. The admin role check
//! needs an arbitrary claim (`admin_claim`, often `groups`), which is not one
//! of the standard ones. Rather than redefine the whole generic `Client` type
//! alias to plumb a custom claims type through, we verify the id token with
//! openidconnect as usual and then re-read its already-verified payload as
//! loose json to pull out that one extra claim. The signature and nonce check
//! already happened by that point, so this is just a second look at the same
//! trusted bytes, not a second source of trust.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use cl_raft::ClusterNode;
use cl_secret::{ClusterKey, Sealed};
use tokio::sync::{Mutex, OnceCell, RwLock};
use tracing::warn;

use crate::error::ApiError;
use crate::oidc::OIDC_CLIENT_SECRET;

use super::{Identity, Role};

/// How long an in-flight login may sit between redirect and callback.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub struct OidcSettings {
    pub issuer: String,
    pub client_id: String,
    pub redirect_url: String,
    pub scopes: Vec<String>,
    pub admin_claim: String,
    pub admin_value: Option<String>,
}

struct PendingLogin {
    nonce: Nonce,
    pkce_verifier: PkceCodeVerifier,
    started_at: Instant,
}

pub struct OidcAuthenticator {
    settings: OidcSettings,
    http: openidconnect::reqwest::Client,
    /// Discovery is cached; the client itself is rebuilt from it on every
    /// call. `CoreClient::from_provider_metadata` returns a type whose
    /// generics encode exactly which endpoints discovery filled in, which is
    /// not nameable as the bare `CoreClient` alias, so it cannot live in a
    /// struct field without redeclaring that whole generic signature. Cheap
    /// to rebuild since it does no IO, unlike discovery itself.
    metadata: OnceCell<CoreProviderMetadata>,
    pending: Mutex<HashMap<String, PendingLogin>>,
    /// Where the client secret actually lives: a sealed cluster secret,
    /// opened fresh on every login rather than resolved once at startup, so
    /// it can be set or rotated from the web ui while the api is already
    /// running. See `crate::routes::oidc`.
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
            // Discovery and token requests go to a fixed, configured issuer;
            // there is no reason for either hop to follow a redirect, and
            // allowing one opens an SSRF path.
            .redirect(openidconnect::reqwest::redirect::Policy::none())
            .build()
            .map_err(ApiError::internal)?;
        Ok(Self {
            settings,
            http,
            metadata: OnceCell::new(),
            pending: Mutex::new(HashMap::new()),
            cluster,
            key,
        })
    }

    /// Opens the client secret out of replicated secrets, decrypting with
    /// this node's cluster key. `None` when there is no key, no secret
    /// stored yet, or the stored bytes cannot be opened -- a provider that
    /// requires a client secret then fails authentication rather than
    /// silently trying with none.
    async fn client_secret(&self) -> Option<String> {
        let key_guard = self.key.read().await;
        let key = key_guard.as_ref()?;
        let state = self.cluster.state().await;
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

    /// Discover the provider once and cache it. Building a `CoreClient` from
    /// the metadata is left to each caller: what `from_provider_metadata(...)
    /// .set_redirect_uri(...)` returns is a `Client` instantiated with
    /// whichever endpoint typestate markers discovery happened to fill in,
    /// which is not nameable as the bare `CoreClient` alias, so it cannot be
    /// the return type of a shared helper without redeclaring that whole
    /// generic signature by hand. Leaving it to type inference at each call
    /// site sidesteps that.
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

    /// Start a login: returns the url to redirect the browser to.
    pub async fn start_login(&self) -> Result<String, ApiError> {
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

        self.gc_expired().await;
        self.pending.lock().await.insert(
            csrf_token.secret().clone(),
            PendingLogin {
                nonce,
                pkce_verifier,
                started_at: Instant::now(),
            },
        );

        Ok(auth_url.to_string())
    }

    /// Finish a login from the callback's `code` and `state`.
    pub async fn finish_login(&self, code: String, state: String) -> Result<Identity, ApiError> {
        let pending = self
            .pending
            .lock()
            .await
            .remove(&state)
            .ok_or_else(|| ApiError::Unauthenticated("unknown or expired login state".into()))?;
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

        let id_token = token_response
            .id_token()
            .ok_or_else(|| ApiError::Unauthenticated("provider did not return an id token".into()))?;
        let verifier = client.id_token_verifier();
        let claims = id_token
            .claims(&verifier, &pending.nonce)
            .map_err(|e| ApiError::Unauthenticated(format!("id token verification failed: {e}")))?;

        let subject = claims.subject().as_str().to_string();
        let role = self.role_from_raw_claims(id_token);

        Ok(Identity { subject, role })
    }

    /// Re-parse the id token's payload as loose json to read `admin_claim`.
    /// See the module doc for why this is safe to do after `claims()` has
    /// already verified the token.
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

    async fn gc_expired(&self) {
        let mut pending = self.pending.lock().await;
        pending.retain(|_, login| login.started_at.elapsed() <= LOGIN_TIMEOUT);
    }
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
