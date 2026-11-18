//! OAuth 2.1 authorization server state for the MCP endpoint.
//!
//! MCP clients register themselves (RFC 7591), send the user through an approval page and trade
//! the code for tokens. This holds everything but the http handlers; [`authenticate_mcp`] is the
//! one call the MCP layer makes to turn a request into a caller.
//!
//! * Client records (with the SHA-256 of each refresh token) live in raft under
//!   [`MCP_CLIENTS_CONFIG`], as [`super::keys`] does, since cl-raft has no `Request` variant for
//!   them. Revoking deletes the record on every node.
//! * Pending requests and codes are leader-only memory: they last seconds to minutes and a raft
//!   write per login is wasteful. A leader change mid-approval restarts the flow.
//! * Access tokens are stateless and HMAC signed, so any node authenticates with only a local
//!   client record check.

use parking_lot::Mutex as StdMutex;
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use cl_config::ClusterConfig;
use cl_raft::ClusterState;
use cl_secret::{internal_auth, ClusterKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::keys::{self, random_token, ApiKeyStore};
use super::{Identity, Role};
use crate::error::ApiError;
use crate::server::AppState;

/// Never exposed through `/api/configs`, git export or backup: it holds refresh token hashes.
pub const MCP_CLIENTS_CONFIG: &str = "_clouduler_mcp_clients";

/// Domain separator: keeps a token signature from ever verifying as an internal-route
/// signature (those labels start `v2\n`/`v3\n`).
const ACCESS_TOKEN_LABEL: &str = "clouduler-mcp-access-token-v1";

pub const ACCESS_TOKEN_PREFIX: &str = "cmt_";
const REFRESH_TOKEN_PREFIX: &str = "cmr_";

pub const ACCESS_TOKEN_TTL: Duration = Duration::from_secs(3600);
const REFRESH_TOKEN_TTL_SECS: i64 = 30 * 24 * 3600;
pub const PENDING_TTL: Duration = Duration::from_secs(10 * 60);
pub const CODE_TTL: Duration = Duration::from_secs(60);
const UNAPPROVED_CLIENT_TTL_SECS: i64 = 24 * 3600;
/// Registration is unauthenticated, so the never-approved records it can leave in raft are capped.
pub const MAX_UNAPPROVED_CLIENTS: usize = 100;
const MAX_IN_MEMORY: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpGrant {
    Read,
    Admin,
}

impl McpGrant {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Self::Read),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    pub fn scope(self) -> &'static str {
        match self {
            Self::Read => "mcp:read",
            Self::Admin => "mcp:read mcp:admin",
        }
    }
}

/// Read per request so toggling needs no restart.
pub fn mcp_enabled(cfg: &ClusterConfig) -> bool {
    cfg.mcp.enabled
}

/// 404 rather than 403 so a disabled cluster looks like one without the feature.
pub async fn require_enabled(state: &AppState) -> Result<(), ApiError> {
    if mcp_enabled(&*state.cluster_config.read().await) {
        Ok(())
    } else {
        Err(ApiError::NotFound("mcp access is disabled".into()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpClient {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub created_at: i64,
    #[serde(default)]
    pub approved_by: Option<String>,
    #[serde(default)]
    pub approver_subject: Option<String>,
    #[serde(default)]
    pub grant: Option<McpGrant>,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub refresh_hash: Option<String>,
    #[serde(default)]
    pub refresh_expires_at: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpClientStore {
    pub clients: Vec<McpClient>,
}

impl McpClientStore {
    pub fn load(state: &ClusterState) -> Self {
        let Some(entry) = state.configs.get(MCP_CLIENTS_CONFIG) else {
            return Self::default();
        };
        let Ok(raw) = entry.decode() else {
            return Self::default();
        };
        serde_json::from_slice(&raw).unwrap_or_default()
    }

    pub fn encode(&self) -> String {
        crate::encode_json_config(self)
    }

    pub fn find(&self, id: &str) -> Option<&McpClient> {
        self.clients.iter().find(|c| c.client_id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut McpClient> {
        self.clients.iter_mut().find(|c| c.client_id == id)
    }

    pub fn prune_unapproved(&mut self, now: i64) {
        self.clients
            .retain(|c| c.approved_by.is_some() || now - c.created_at < UNAPPROVED_CLIENT_TTL_SECS);
    }

    pub fn unapproved(&self) -> usize {
        self.clients
            .iter()
            .filter(|c| c.approved_by.is_none())
            .count()
    }
}

#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uri: String,
    pub state: Option<String>,
    pub code_challenge: String,
    pub scope: String,
    pub resource: Option<String>,
    created: Instant,
}

#[derive(Debug, Clone)]
pub struct AuthCode {
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub resource: Option<String>,
    pub identity: Identity,
    pub grant: McpGrant,
    created: Instant,
}

#[derive(Default)]
pub struct OAuthMemory {
    pending: StdMutex<HashMap<String, PendingRequest>>,
    codes: StdMutex<HashMap<String, AuthCode>>,
    pub write_lock: tokio::sync::Mutex<()>,
}

fn lock<T>(m: &StdMutex<T>) -> parking_lot::MutexGuard<'_, T> {
    m.lock()
}

impl OAuthMemory {
    pub fn add_pending(
        &self,
        client: &McpClient,
        redirect_uri: String,
        state: Option<String>,
        code_challenge: String,
        scope: String,
        resource: Option<String>,
    ) -> Result<String, ApiError> {
        let mut map = lock(&self.pending);
        map.retain(|_, p| p.created.elapsed() < PENDING_TTL);
        if map.len() >= MAX_IN_MEMORY {
            return Err(ApiError::TooManyRequests(
                "too many pending authorization requests".into(),
            ));
        }
        let id = random_token(32);
        map.insert(
            id.clone(),
            PendingRequest {
                client_id: client.client_id.clone(),
                client_name: client.client_name.clone(),
                redirect_uri,
                state,
                code_challenge,
                scope,
                resource,
                created: Instant::now(),
            },
        );
        Ok(id)
    }

    pub fn list_pending(&self) -> Vec<(String, PendingRequest, u64)> {
        let map = lock(&self.pending);
        let mut rows: Vec<_> = map
            .iter()
            .filter(|(_, p)| p.created.elapsed() < PENDING_TTL)
            .map(|(id, p)| {
                let left = PENDING_TTL.saturating_sub(p.created.elapsed()).as_secs();
                (id.clone(), p.clone(), left)
            })
            .collect();
        rows.sort_by_key(|(_, _, left)| std::cmp::Reverse(*left));
        rows
    }

    pub fn peek_pending(&self, id: &str) -> Option<PendingRequest> {
        let map = lock(&self.pending);
        map.get(id)
            .filter(|p| p.created.elapsed() < PENDING_TTL)
            .cloned()
    }

    pub fn take_pending(&self, id: &str) -> Option<PendingRequest> {
        lock(&self.pending)
            .remove(id)
            .filter(|p| p.created.elapsed() < PENDING_TTL)
    }

    pub fn issue_code(
        &self,
        pending: &PendingRequest,
        identity: Identity,
        grant: McpGrant,
    ) -> String {
        let mut map = lock(&self.codes);
        map.retain(|_, c| c.created.elapsed() < CODE_TTL);
        let code = random_token(43);
        map.insert(
            code.clone(),
            AuthCode {
                client_id: pending.client_id.clone(),
                redirect_uri: pending.redirect_uri.clone(),
                code_challenge: pending.code_challenge.clone(),
                resource: pending.resource.clone(),
                identity,
                grant,
                created: Instant::now(),
            },
        );
        code
    }

    /// Single use: a presented code is consumed even if the exchange fails, so a stolen one cannot be guessed against.
    pub fn take_code(&self, code: &str) -> Option<AuthCode> {
        lock(&self.codes)
            .remove(code)
            .filter(|c| c.created.elapsed() < CODE_TTL)
    }

    pub fn prune_expired(&self) {
        lock(&self.pending).retain(|_, p| p.created.elapsed() < PENDING_TTL);
        lock(&self.codes).retain(|_, c| c.created.elapsed() < CODE_TTL);
    }
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn new_refresh_token(client_id: &str) -> String {
    format!("{REFRESH_TOKEN_PREFIX}{client_id}.{}", random_token(48))
}

pub fn refresh_token_client(token: &str) -> Option<&str> {
    token
        .strip_prefix(REFRESH_TOKEN_PREFIX)?
        .split_once('.')
        .map(|(id, _)| id)
}

pub fn refresh_expiry(now: i64) -> i64 {
    now + REFRESH_TOKEN_TTL_SECS
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenPayload {
    cid: String,
    sub: String,
    usr: String,
    grant: McpGrant,
    generation: u64,
    exp: i64,
}

#[derive(Debug, Clone)]
pub struct OAuthGrant {
    pub identity: Identity,
    pub client_id: String,
    pub grant: McpGrant,
}

/// A read grant is a viewer even when the approving human is an admin, keeping read-only clients out of admin paths.
pub fn identity_for(subject: &str, username: &str, grant: McpGrant) -> Identity {
    Identity {
        subject: subject.to_string(),
        username: username.to_string(),
        role: match grant {
            McpGrant::Admin => Role::Admin,
            McpGrant::Read => Role::Viewer,
        },
    }
}

pub fn sign_access_token(
    key: &ClusterKey,
    client: &McpClient,
    subject: &str,
    username: &str,
    grant: McpGrant,
    now: i64,
) -> String {
    let payload = TokenPayload {
        cid: client.client_id.clone(),
        sub: subject.to_string(),
        usr: username.to_string(),
        grant,
        generation: client.generation,
        exp: now + ACCESS_TOKEN_TTL.as_secs() as i64,
    };
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("payload serializes"));
    let sig = internal_auth::sign(key, &format!("{ACCESS_TOKEN_LABEL}\n{body}"));
    format!("{ACCESS_TOKEN_PREFIX}{body}.{sig}")
}

fn decode_access_token(key: &ClusterKey, token: &str, now: i64) -> Result<TokenPayload, ApiError> {
    let bad = || ApiError::Unauthenticated("invalid access token".into());
    let rest = token.strip_prefix(ACCESS_TOKEN_PREFIX).ok_or_else(bad)?;
    let (body, sig) = rest.split_once('.').ok_or_else(bad)?;
    if !internal_auth::verify(key, &format!("{ACCESS_TOKEN_LABEL}\n{body}"), sig) {
        return Err(bad());
    }
    let raw = URL_SAFE_NO_PAD.decode(body).map_err(|_| bad())?;
    let payload: TokenPayload = serde_json::from_slice(&raw).map_err(|_| bad())?;
    if payload.exp <= now {
        return Err(ApiError::Unauthenticated("access token expired".into()));
    }
    Ok(payload)
}

/// The record check (exists, approved, same generation) is what makes revocation immediate for a stateless token.
pub async fn verify_access_token(state: &AppState, token: &str) -> Result<OAuthGrant, ApiError> {
    let key = state
        .key
        .read()
        .await
        .clone()
        .ok_or_else(|| ApiError::Unavailable("this node holds no cluster key".into()))?;
    let payload = decode_access_token(&key, token, now_secs())?;
    let cluster_state = state.cluster.state().await;
    let store = McpClientStore::load(&cluster_state);
    match store.find(&payload.cid) {
        Some(c) if c.approved_by.is_some() && c.generation == payload.generation => {}
        _ => return Err(ApiError::Unauthenticated("access token revoked".into())),
    }
    Ok(OAuthGrant {
        identity: identity_for(&payload.sub, &payload.usr, payload.grant),
        client_id: payload.cid,
        grant: payload.grant,
    })
}

#[derive(Debug, Clone)]
pub struct McpCaller {
    pub identity: Identity,
    pub grant: McpGrant,
    pub client: Option<String>,
}

/// Accepts an OAuth token (`cmt_...`) or an api key (`clk_...`, always admin). `NotFound` when MCP
/// is disabled; otherwise `Unauthenticated`, to be turned into a response with [`challenge`].
pub async fn authenticate_mcp(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<McpCaller, ApiError> {
    require_enabled(state).await?;
    let value = headers
        .get(header::AUTHORIZATION)
        .ok_or_else(|| ApiError::Unauthenticated("no credentials presented".into()))?
        .to_str()
        .map_err(|_| ApiError::Unauthenticated("malformed authorization header".into()))?;
    let token = value
        .strip_prefix("Bearer ")
        .ok_or_else(|| ApiError::Unauthenticated("malformed authorization header".into()))?
        .trim();

    if token.starts_with(ACCESS_TOKEN_PREFIX) {
        let g = verify_access_token(state, token).await?;
        return Ok(McpCaller {
            identity: g.identity,
            grant: g.grant,
            client: Some(g.client_id),
        });
    }
    if let Some((id, secret)) = keys::parse(token) {
        let cluster_state = state.cluster.state().await;
        let store = ApiKeyStore::load(&cluster_state);
        return keys::verify(&store, id, secret)
            .map(|record| McpCaller {
                identity: Identity {
                    subject: record.id.clone(),
                    username: record.name.clone().unwrap_or_else(|| record.id.clone()),
                    role: Role::Admin,
                },
                grant: McpGrant::Admin,
                client: None,
            })
            .ok_or_else(|| ApiError::Unauthenticated("invalid api key".into()));
    }
    Err(ApiError::Unauthenticated(
        "unrecognized bearer token".into(),
    ))
}

/// Points a client at the protected resource metadata (RFC 9728).
pub fn www_authenticate(cfg: &ClusterConfig) -> String {
    format!(
        "Bearer resource_metadata=\"https://{}/.well-known/oauth-protected-resource\"",
        cfg.web_fqdn()
    )
}

pub async fn challenge(state: &AppState, err: ApiError) -> Response {
    let is_401 = matches!(err, ApiError::Unauthenticated(_));
    let mut resp = err.into_response();
    if is_401 {
        let value = www_authenticate(&*state.cluster_config.read().await);
        if let Ok(v) = header::HeaderValue::from_str(&value) {
            resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> McpClient {
        McpClient {
            client_id: "c1".into(),
            client_name: "test".into(),
            redirect_uris: vec![],
            created_at: 0,
            approved_by: Some("a".into()),
            approver_subject: Some("a".into()),
            grant: Some(McpGrant::Read),
            generation: 3,
            refresh_hash: None,
            refresh_expires_at: None,
        }
    }

    #[test]
    fn a_signed_token_round_trips() {
        let key = ClusterKey::generate();
        let t = sign_access_token(&key, &client(), "sub", "user", McpGrant::Admin, 1000);
        let p = decode_access_token(&key, &t, 1001).unwrap();
        assert_eq!(p.cid, "c1");
        assert_eq!(p.sub, "sub");
        assert_eq!(p.usr, "user");
        assert_eq!(p.grant, McpGrant::Admin);
        assert_eq!(p.generation, 3);
    }

    #[test]
    fn an_expired_token_is_rejected() {
        let key = ClusterKey::generate();
        let t = sign_access_token(&key, &client(), "s", "u", McpGrant::Read, 1000);
        assert!(decode_access_token(&key, &t, 1000 + 3600).is_err());
        assert!(decode_access_token(&key, &t, 1000 + 3599).is_ok());
    }

    #[test]
    fn a_tampered_token_is_rejected() {
        let key = ClusterKey::generate();
        let t = sign_access_token(&key, &client(), "s", "u", McpGrant::Read, 1000);
        let (body, sig) = t
            .strip_prefix(ACCESS_TOKEN_PREFIX)
            .unwrap()
            .split_once('.')
            .unwrap();
        let mut p: TokenPayload =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).unwrap()).unwrap();
        p.grant = McpGrant::Admin;
        let forged = format!(
            "{ACCESS_TOKEN_PREFIX}{}.{sig}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&p).unwrap())
        );
        assert!(decode_access_token(&key, &forged, 1001).is_err());
        assert!(decode_access_token(&key, "cmt_garbage", 1001).is_err());
    }

    #[test]
    fn another_clusters_key_does_not_verify() {
        let t = sign_access_token(
            &ClusterKey::generate(),
            &client(),
            "s",
            "u",
            McpGrant::Read,
            0,
        );
        assert!(decode_access_token(&ClusterKey::generate(), &t, 1).is_err());
    }

    #[test]
    fn pkce_matches_the_rfc_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn codes_are_single_use() {
        let mem = OAuthMemory::default();
        let id = mem
            .add_pending(
                &client(),
                "https://x/cb".into(),
                None,
                "ch".into(),
                "".into(),
                None,
            )
            .unwrap();
        let p = mem.take_pending(&id).unwrap();
        assert!(mem.take_pending(&id).is_none());
        let identity = identity_for("s", "u", McpGrant::Read);
        let code = mem.issue_code(&p, identity, McpGrant::Read);
        assert!(mem.take_code(&code).is_some());
        assert!(mem.take_code(&code).is_none());
    }
}
