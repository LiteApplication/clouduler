//! OAuth 2.1 authorization server for MCP clients, plus the ui's consent
//! and client-management endpoints.
//!
//! Layout of the flow: a client discovers us through the `.well-known`
//! documents, registers (`/oauth/register`), sends the user's browser to
//! `/oauth/authorize`, which parks the request in memory and bounces to the
//! ui's consent page. The ui, logged in as the human, approves through
//! `/api/oauth/requests/{id}/approve`, which mints a one-shot code and hands
//! back the redirect for the browser to follow. The client then redeems the
//! code at `/oauth/token`. All state beyond the client records lives on the
//! leader, see `auth::oauth`.
//!
//! Every endpoint but `/api/mcp/clients*` answers 404 when `mcp.enabled` is
//! false; the client list stays reachable so an admin can still see and
//! revoke what was granted before the switch was flipped.

use axum::body::Bytes;
use axum::extract::{Form, Path, Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use cl_raft::Request as ClusterRequest;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::keys::random_token;
use crate::auth::oauth::{
    self, ct_eq, now_secs, pkce_challenge, require_enabled, sha256_hex, McpClient, McpClientStore,
    McpGrant, MAX_UNAPPROVED_CLIENTS, MCP_CLIENTS_CONFIG,
};
use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
        .route(
            "/.well-known/oauth-authorization-server/mcp",
            get(authorization_server),
        )
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/token", post(token))
        .route("/api/oauth/requests", get(list_requests))
        .route("/api/oauth/requests/{id}", get(request_info))
        .route("/api/oauth/requests/{id}/approve", post(approve))
        .route("/api/oauth/requests/{id}/deny", post(deny))
        .route("/api/mcp/clients", get(list_clients))
        .route("/api/mcp/clients/{id}", delete(revoke_client))
}

// ---- errors ----

/// Either an RFC 6749 section 5.2 error body or a plain [`ApiError`] (used
/// for the 404 when disabled and for infrastructure failures, which have no
/// oauth code).
enum OAuthError {
    Oauth {
        status: StatusCode,
        error: &'static str,
        description: String,
    },
    Api(ApiError),
}

impl OAuthError {
    fn new(error: &'static str, description: impl Into<String>) -> Self {
        Self::Oauth {
            status: StatusCode::BAD_REQUEST,
            error,
            description: description.into(),
        }
    }
}

impl From<ApiError> for OAuthError {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl IntoResponse for OAuthError {
    fn into_response(self) -> Response {
        match self {
            Self::Api(e) => e.into_response(),
            Self::Oauth {
                status,
                error,
                description,
            } => {
                let mut resp = (
                    status,
                    Json(json!({"error": error, "error_description": description})),
                )
                    .into_response();
                no_store(&mut resp);
                resp
            }
        }
    }
}

fn no_store(resp: &mut Response) {
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

// ---- discovery ----

async fn issuer_and_resource(state: &AppState) -> Result<(String, String), ApiError> {
    require_enabled(state).await?;
    let fqdn = state.cluster_config.read().await.web_fqdn();
    Ok((format!("https://{fqdn}"), format!("https://{fqdn}/mcp")))
}

async fn protected_resource(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (issuer, resource) = issuer_and_resource(&state).await?;
    Ok(Json(json!({
        "resource": resource,
        "authorization_servers": [issuer],
        "bearer_methods_supported": ["header"],
        "scopes_supported": ["mcp:read", "mcp:admin"],
    })))
}

async fn authorization_server(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (issuer, _) = issuer_and_resource(&state).await?;
    Ok(Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/oauth/authorize"),
        "token_endpoint": format!("{issuer}/oauth/token"),
        "registration_endpoint": format!("{issuer}/oauth/register"),
        "scopes_supported": ["mcp:read", "mcp:admin"],
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
    })))
}

// ---- persistence ----

async fn submit_store(state: &AppState, store: &McpClientStore) -> Result<(), ApiError> {
    let response = state
        .cluster
        .submit(ClusterRequest::PutConfig {
            name: MCP_CLIENTS_CONFIG.to_string(),
            content: store.encode(),
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Internal(msg));
    }
    Ok(())
}

async fn load_store(state: &AppState) -> McpClientStore {
    McpClientStore::load(&state.cluster.state().await)
}

// ---- redirect uri handling ----

/// Hosts a plain-http redirect may point at: loopback only, which is what
/// native apps (Claude Code, MCP inspector) listen on.
fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// `(scheme, authority)` of a redirect uri, or `None` if it is not
/// acceptable for a public client to register.
///
/// https is accepted anywhere, http only on loopback, and other schemes are
/// private-use app schemes (`cursor://`, `vscode://`) per RFC 8252. Schemes
/// that can run script or exfiltrate in a browser are refused outright, and
/// fragments and userinfo are never allowed.
fn parse_redirect_uri(uri: &str) -> Option<(String, String)> {
    if uri.len() > 2048 || uri.contains('#') || uri.chars().any(|c| c.is_control() || c == ' ') {
        return None;
    }
    let (scheme, rest) = uri.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let mut chars = scheme.chars();
    if !chars.next()?.is_ascii_lowercase()
        || !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "+.-".contains(c))
    {
        return None;
    }
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    if authority.contains('@') {
        return None;
    }
    match scheme.as_str() {
        "https" => (!authority.is_empty()).then(|| (scheme, authority.to_string())),
        "http" => {
            let host = match authority.rsplit_once(':') {
                Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
                _ => authority,
            };
            is_loopback_host(&host.to_ascii_lowercase()).then(|| (scheme, authority.to_string()))
        }
        "javascript" | "data" | "file" | "ftp" | "blob" | "about" | "vbscript" | "ws" | "wss" => {
            None
        }
        _ => Some((scheme, authority.to_string())),
    }
}

/// What the consent page shows as "where this will send you".
fn display_target(uri: &str) -> String {
    match parse_redirect_uri(uri) {
        Some((scheme, authority)) if scheme == "https" || scheme == "http" => authority,
        Some((scheme, authority)) => format!("{scheme}://{authority}"),
        None => uri.to_string(),
    }
}

fn with_query(uri: &str, params: &[(&str, &str)]) -> String {
    let query = serde_urlencoded::to_string(params).unwrap_or_default();
    let sep = if uri.contains('?') { '&' } else { '?' };
    format!("{uri}{sep}{query}")
}

// ---- dynamic client registration ----

#[derive(Deserialize)]
struct RegisterRequest {
    #[serde(default)]
    client_name: Option<String>,
    #[serde(default)]
    redirect_uris: Vec<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
    #[serde(default)]
    grant_types: Option<Vec<String>>,
    #[serde(default)]
    response_types: Option<Vec<String>>,
}

fn invalid_metadata(msg: impl Into<String>) -> OAuthError {
    OAuthError::new("invalid_client_metadata", msg)
}

async fn register(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> Result<Response, OAuthError> {
    require_enabled(&state).await?;
    if body
        .token_endpoint_auth_method
        .as_deref()
        .is_some_and(|m| m != "none")
    {
        return Err(invalid_metadata(
            "only public clients (token_endpoint_auth_method none) are supported",
        ));
    }
    if body.grant_types.as_ref().is_some_and(|g| {
        g.iter()
            .any(|g| g != "authorization_code" && g != "refresh_token")
    }) {
        return Err(invalid_metadata("unsupported grant_types"));
    }
    if body
        .response_types
        .as_ref()
        .is_some_and(|r| r.iter().any(|r| r != "code"))
    {
        return Err(invalid_metadata("unsupported response_types"));
    }
    if body.redirect_uris.is_empty() || body.redirect_uris.len() > 10 {
        return Err(invalid_metadata(
            "between 1 and 10 redirect_uris are required",
        ));
    }
    for uri in &body.redirect_uris {
        if parse_redirect_uri(uri).is_none() {
            return Err(OAuthError::new(
                "invalid_redirect_uri",
                format!("redirect uri not allowed: {uri}"),
            ));
        }
    }
    let name: String = body
        .client_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("Unnamed MCP client")
        .chars()
        .filter(|c| !c.is_control())
        .take(100)
        .collect();

    let _guard = state.oauth.write_lock.lock().await;
    let mut store = load_store(&state).await;
    let now = now_secs();
    store.prune_unapproved(now);
    if store.unapproved() >= MAX_UNAPPROVED_CLIENTS {
        return Err(
            ApiError::TooManyRequests("too many unapproved clients registered".into()).into(),
        );
    }
    let client = McpClient {
        client_id: format!("mcp_{}", random_token(24)),
        client_name: name,
        redirect_uris: body.redirect_uris,
        created_at: now,
        approved_by: None,
        approver_subject: None,
        grant: None,
        generation: 0,
        refresh_hash: None,
        refresh_expires_at: None,
    };
    store.clients.push(client.clone());
    submit_store(&state, &store).await?;

    let mut resp = (
        StatusCode::CREATED,
        Json(json!({
            "client_id": client.client_id,
            "client_name": client.client_name,
            "redirect_uris": client.redirect_uris,
            "client_id_issued_at": now,
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
        })),
    )
        .into_response();
    no_store(&mut resp);
    Ok(resp)
}

// ---- authorize ----

#[derive(Deserialize)]
struct AuthorizeParams {
    client_id: Option<String>,
    redirect_uri: Option<String>,
    response_type: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    scope: Option<String>,
    resource: Option<String>,
}

fn resource_matches(given: &str, expected: &str) -> bool {
    given.trim_end_matches('/') == expected.trim_end_matches('/')
}

async fn authorize(
    State(state): State<AppState>,
    Query(p): Query<AuthorizeParams>,
) -> Result<Response, ApiError> {
    require_enabled(&state).await?;

    // Until the client and redirect uri check out, an error must NOT be sent
    // to the redirect uri, or this endpoint becomes an open redirector.
    let client_id = p
        .client_id
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("client_id is required".into()))?;
    let store = load_store(&state).await;
    let client = store
        .find(client_id)
        .ok_or_else(|| ApiError::BadRequest("unknown client_id".into()))?;
    let redirect_uri = match p.redirect_uri.as_deref() {
        Some(uri) if client.redirect_uris.iter().any(|r| r == uri) => uri.to_string(),
        Some(_) => {
            return Err(ApiError::BadRequest(
                "redirect_uri is not registered".into(),
            ))
        }
        None if client.redirect_uris.len() == 1 => client.redirect_uris[0].clone(),
        None => return Err(ApiError::BadRequest("redirect_uri is required".into())),
    };

    let fail = |error: &str, desc: &str| -> Result<Response, ApiError> {
        let mut params = vec![("error", error), ("error_description", desc)];
        if let Some(s) = p.state.as_deref() {
            params.push(("state", s));
        }
        Ok(Redirect::to(&with_query(&redirect_uri, &params)).into_response())
    };

    if p.response_type.as_deref() != Some("code") {
        return fail("unsupported_response_type", "response_type must be code");
    }
    let Some(challenge) = p.code_challenge.as_deref().filter(|c| !c.is_empty()) else {
        return fail("invalid_request", "code_challenge is required");
    };
    if p.code_challenge_method.as_deref() != Some("S256") {
        return fail("invalid_request", "code_challenge_method must be S256");
    }
    let (_, resource) = issuer_and_resource(&state).await?;
    if let Some(given) = p.resource.as_deref() {
        if !resource_matches(given, &resource) {
            return fail("invalid_target", "resource does not match this server");
        }
    }

    let id = state.oauth.add_pending(
        client,
        redirect_uri,
        p.state.clone(),
        challenge.to_string(),
        p.scope.clone().unwrap_or_default(),
        p.resource.clone(),
    )?;
    Ok(Redirect::to(&format!("/oauth/consent?req={id}")).into_response())
}

// ---- consent (called by the ui) ----

#[derive(Serialize)]
struct RequestInfo {
    client_name: String,
    redirect_host: String,
    scope: String,
    /// What the requested scope suggests; the user still chooses.
    requested_grant: McpGrant,
    allowed_grants: Vec<McpGrant>,
}

#[derive(Serialize)]
struct PendingRow {
    id: String,
    client_name: String,
    redirect_host: String,
    scope: String,
    requested_grant: McpGrant,
    /// Seconds until the request expires.
    expires_in: u64,
    /// The node holding the request. Pending requests live in the memory of
    /// the node that received the authorize call, so this list only covers
    /// the node answering.
    node: String,
}

/// Authorization requests waiting for someone to approve them, so the ui can
/// show them without the user having the consent link.
async fn list_requests(
    admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
) -> Result<Json<Vec<PendingRow>>, ApiError> {
    let _ = admin;
    require_enabled(&state).await?;
    Ok(Json(
        state
            .oauth
            .list_pending()
            .into_iter()
            .map(|(id, p, expires_in)| {
                let wants_admin = p.scope.split_whitespace().any(|s| s == "mcp:admin");
                PendingRow {
                    id,
                    client_name: p.client_name,
                    redirect_host: display_target(&p.redirect_uri),
                    scope: p.scope,
                    requested_grant: if wants_admin {
                        McpGrant::Admin
                    } else {
                        McpGrant::Read
                    },
                    expires_in,
                    node: state.node_name.clone(),
                }
            })
            .collect(),
    ))
}

fn allowed_grants(identity: &Identity) -> Vec<McpGrant> {
    if identity.role.is_admin() {
        vec![McpGrant::Read, McpGrant::Admin]
    } else {
        vec![McpGrant::Read]
    }
}

async fn request_info(
    identity: Identity,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<RequestInfo>, ApiError> {
    require_enabled(&state).await?;
    let pending = state
        .oauth
        .peek_pending(&id)
        .ok_or_else(|| ApiError::NotFound("this authorization request expired".into()))?;
    let wants_admin = pending.scope.split_whitespace().any(|s| s == "mcp:admin");
    Ok(Json(RequestInfo {
        client_name: pending.client_name,
        redirect_host: display_target(&pending.redirect_uri),
        scope: pending.scope,
        requested_grant: if wants_admin {
            McpGrant::Admin
        } else {
            McpGrant::Read
        },
        allowed_grants: allowed_grants(&identity),
    }))
}

#[derive(Deserialize)]
struct ApproveBody {
    grant: String,
}

async fn approve(
    identity: Identity,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ApproveBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_enabled(&state).await?;
    let grant = McpGrant::parse(&body.grant)
        .ok_or_else(|| ApiError::BadRequest("grant must be read or admin".into()))?;
    if !allowed_grants(&identity).contains(&grant) {
        // Checked before the request is consumed, so the user can retry with
        // a grant they are allowed to give.
        return Err(ApiError::Forbidden(
            "only an admin can grant admin access".into(),
        ));
    }
    let pending = state
        .oauth
        .take_pending(&id)
        .ok_or_else(|| ApiError::NotFound("this authorization request expired".into()))?;
    let code = state.oauth.issue_code(&pending, identity, grant);
    let mut params = vec![("code", code.as_str())];
    if let Some(s) = pending.state.as_deref() {
        params.push(("state", s));
    }
    Ok(Json(
        json!({"redirect": with_query(&pending.redirect_uri, &params)}),
    ))
}

async fn deny(
    _identity: Identity,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_enabled(&state).await?;
    let pending = state
        .oauth
        .take_pending(&id)
        .ok_or_else(|| ApiError::NotFound("this authorization request expired".into()))?;
    let mut params = vec![("error", "access_denied")];
    if let Some(s) = pending.state.as_deref() {
        params.push(("state", s));
    }
    Ok(Json(
        json!({"redirect": with_query(&pending.redirect_uri, &params)}),
    ))
}

// ---- token ----

#[derive(Deserialize)]
struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    resource: Option<String>,
}

fn invalid_grant(msg: &str) -> OAuthError {
    OAuthError::new("invalid_grant", msg)
}

fn invalid_client() -> OAuthError {
    OAuthError::Oauth {
        status: StatusCode::UNAUTHORIZED,
        error: "invalid_client",
        description: "unknown client".into(),
    }
}

fn token_response(access: String, refresh: String, grant: McpGrant) -> Response {
    let mut resp = Json(json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": oauth::ACCESS_TOKEN_TTL.as_secs(),
        "refresh_token": refresh,
        "scope": grant.scope(),
    }))
    .into_response();
    no_store(&mut resp);
    resp.headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    resp
}

async fn token(
    State(state): State<AppState>,
    Form(f): Form<TokenForm>,
) -> Result<Response, OAuthError> {
    require_enabled(&state).await?;
    let key = state
        .key
        .read()
        .await
        .clone()
        .ok_or_else(|| ApiError::Unavailable("this node holds no cluster key".into()))?;
    let (_, resource) = issuer_and_resource(&state).await?;
    if let Some(given) = f.resource.as_deref() {
        if !resource_matches(given, &resource) {
            return Err(OAuthError::new(
                "invalid_target",
                "resource does not match this server",
            ));
        }
    }
    let now = now_secs();

    match f.grant_type.as_deref() {
        Some("authorization_code") => {
            let code = f
                .code
                .as_deref()
                .ok_or_else(|| OAuthError::new("invalid_request", "code is required"))?;
            let client_id = f
                .client_id
                .as_deref()
                .ok_or_else(|| OAuthError::new("invalid_request", "client_id is required"))?;
            let verifier = f
                .code_verifier
                .as_deref()
                .ok_or_else(|| OAuthError::new("invalid_request", "code_verifier is required"))?;

            // Burns the code even if a check below fails.
            let auth = state
                .oauth
                .take_code(code)
                .ok_or_else(|| invalid_grant("code is invalid, expired or already used"))?;
            if auth.client_id != client_id {
                return Err(invalid_grant("code was issued to another client"));
            }
            if f.redirect_uri.as_deref() != Some(auth.redirect_uri.as_str()) {
                return Err(invalid_grant(
                    "redirect_uri does not match the authorization request",
                ));
            }
            if !ct_eq(&pkce_challenge(verifier), &auth.code_challenge) {
                return Err(invalid_grant("PKCE verification failed"));
            }
            if let (Some(given), Some(bound)) = (f.resource.as_deref(), auth.resource.as_deref()) {
                if !resource_matches(given, bound) {
                    return Err(OAuthError::new(
                        "invalid_target",
                        "resource differs from the authorization request",
                    ));
                }
            }

            let _guard = state.oauth.write_lock.lock().await;
            let mut store = load_store(&state).await;
            let client = store.find_mut(client_id).ok_or_else(invalid_client)?;
            client.approved_by = Some(auth.identity.username.clone());
            client.approver_subject = Some(auth.identity.subject.clone());
            client.grant = Some(auth.grant);
            // New approval: access tokens from any earlier one stop working.
            client.generation += 1;
            let refresh = oauth::new_refresh_token(client_id);
            client.refresh_hash = Some(sha256_hex(&refresh));
            client.refresh_expires_at = Some(oauth::refresh_expiry(now));
            let access = oauth::sign_access_token(
                &key,
                client,
                &auth.identity.subject,
                &auth.identity.username,
                auth.grant,
                now,
            );
            submit_store(&state, &store).await?;
            Ok(token_response(access, refresh, auth.grant))
        }
        Some("refresh_token") => {
            let presented = f
                .refresh_token
                .as_deref()
                .ok_or_else(|| OAuthError::new("invalid_request", "refresh_token is required"))?;
            let client_id = oauth::refresh_token_client(presented)
                .ok_or_else(|| invalid_grant("malformed refresh token"))?;
            if f.client_id.as_deref().is_some_and(|c| c != client_id) {
                return Err(invalid_grant("refresh token belongs to another client"));
            }

            let _guard = state.oauth.write_lock.lock().await;
            let mut store = load_store(&state).await;
            let client = store
                .find_mut(client_id)
                .ok_or_else(|| invalid_grant("refresh token revoked"))?;
            let valid = client
                .refresh_hash
                .as_deref()
                .is_some_and(|h| ct_eq(h, &sha256_hex(presented)))
                && client.refresh_expires_at.is_some_and(|e| e > now);
            if !valid {
                return Err(invalid_grant(
                    "refresh token is invalid, expired or already used",
                ));
            }
            let (Some(grant), Some(subject), Some(username)) = (
                client.grant,
                client.approver_subject.clone(),
                client.approved_by.clone(),
            ) else {
                return Err(invalid_grant("client is not approved"));
            };
            let refresh = oauth::new_refresh_token(client_id);
            client.refresh_hash = Some(sha256_hex(&refresh));
            client.refresh_expires_at = Some(oauth::refresh_expiry(now));
            let access = oauth::sign_access_token(&key, client, &subject, &username, grant, now);
            submit_store(&state, &store).await?;
            Ok(token_response(access, refresh, grant))
        }
        Some(_) => Err(OAuthError::new(
            "unsupported_grant_type",
            "use authorization_code or refresh_token",
        )),
        None => Err(OAuthError::new("invalid_request", "grant_type is required")),
    }
}

// ---- client management ----

#[derive(Serialize)]
struct ClientSummary {
    client_id: String,
    client_name: String,
    grant: McpGrant,
    approved_by: String,
    created_at: i64,
}

async fn list_clients(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Vec<ClientSummary>> {
    let store = load_store(&state).await;
    Json(
        store
            .clients
            .into_iter()
            .filter_map(|c| {
                Some(ClientSummary {
                    grant: c.grant?,
                    approved_by: c.approved_by?,
                    client_id: c.client_id,
                    client_name: c.client_name,
                    created_at: c.created_at,
                })
            })
            .collect(),
    )
}

async fn revoke_client(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let _guard = state.oauth.write_lock.lock().await;
    let mut store = load_store(&state).await;
    let before = store.clients.len();
    store.clients.retain(|c| c.client_id != id);
    if store.clients.len() == before {
        return Err(ApiError::NotFound(format!("no client {id}")));
    }
    submit_store(&state, &store).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- follower support ----

/// Relay a `/oauth/register` or `/oauth/token` POST to the leader.
///
/// An MCP client reaches whichever node the dashboard hostname resolves to
/// and will not follow a 503 + Location the way the ui does. The authorization
/// codes those calls depend on only exist in the leader's memory, so the
/// follower proxies the request instead of refusing it.
pub(crate) async fn forward_to_leader(state: &AppState, request: Request) -> Response {
    let Some(leader) = crate::server::leader_location(state).await else {
        return ApiError::NotLeader { leader: None }.into_response();
    };
    let (parts, body) = request.into_parts();
    let Ok(bytes): Result<Bytes, _> = axum::body::to_bytes(body, 64 * 1024).await else {
        return ApiError::BadRequest("request body too large".into()).into_response();
    };
    let url = format!("{}{}", leader.trim_end_matches('/'), parts.uri.path());
    let mut req = state.http.post(url).body(bytes);
    if let Some(ct) = parts.headers.get(header::CONTENT_TYPE) {
        req = req.header(header::CONTENT_TYPE, ct.as_bytes());
    }
    match req.send().await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let content_type = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| HeaderValue::from_bytes(v.as_bytes()).ok());
            let body = resp.bytes().await.unwrap_or_default();
            let mut out = (status, body).into_response();
            if let Some(ct) = content_type {
                out.headers_mut().insert(header::CONTENT_TYPE, ct);
            }
            no_store(&mut out);
            out
        }
        Err(e) => ApiError::Unavailable(format!("could not reach the leader: {e}")).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::oauth::identity_for;

    #[test]
    fn redirect_uris_follow_the_registration_rules() {
        for ok in [
            "https://claude.ai/api/mcp/auth_callback",
            "http://localhost:33418/callback",
            "http://127.0.0.1/cb",
            "http://[::1]:8080/cb",
            "cursor://anysphere.cursor-retrieval/oauth/callback",
            "vscode://ms-vscode.mcp/callback",
        ] {
            assert!(parse_redirect_uri(ok).is_some(), "{ok}");
        }
        for bad in [
            "http://example.com/cb",
            "http://localhost.evil.com/cb",
            "javascript://x/%0aalert(1)",
            "https://a.com/cb#frag",
            "https://user@a.com/cb",
            "not a uri",
            "https:///nohost",
            "data://x",
        ] {
            assert!(parse_redirect_uri(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn query_params_are_appended_with_the_right_separator() {
        assert_eq!(
            with_query("https://a/cb", &[("code", "x y")]),
            "https://a/cb?code=x+y"
        );
        assert_eq!(
            with_query("https://a/cb?z=1", &[("code", "x")]),
            "https://a/cb?z=1&code=x"
        );
    }

    #[test]
    fn identity_for_a_viewer_grant_is_a_viewer() {
        assert!(!identity_for("s", "u", McpGrant::Read).role.is_admin());
    }
}
