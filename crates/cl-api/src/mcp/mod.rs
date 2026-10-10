//! MCP server for AI clients, served at `/mcp` over streamable HTTP.
//!
//! An AI client (Claude Code, claude.ai, Cursor...) connects with an OAuth
//! token or api key and gets tools to see cluster state and problems, read
//! logs, build and fix stacks, and debug inside containers, without ever being
//! able to read a secret value. See docs/mcp.md.
//!
//! The protocol, sessions and transport come from `rmcp`; this module adds
//! what it cannot know about:
//!
//! * authentication happens on every HTTP request, before rmcp sees it, and
//!   the verified [`McpCaller`] travels in the request extensions that rmcp
//!   hands to the handler, so a revoked client stops working mid-session;
//! * every result passes through [`finish`], the single place that redacts
//!   secrets, bounds the size and appends warnings, so no tool has to
//!   remember to;
//! * a follower relays `/mcp` to the leader (see [`forward_to_leader`]),
//!   because sessions and focus live in the serving node's memory.

mod problems;
pub mod redact;
mod resources;
mod session;
mod tools;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderName};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData as McpError,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, ServerCapabilities,
    ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{RoleServer, ServerHandler};

use crate::auth::oauth::{authenticate_mcp, challenge, McpCaller, McpGrant};
use crate::error::ApiError;
use crate::server::AppState;
use redact::Redactor;
use session::Sessions;
use tools::Ctx;

/// A tool result is cut to this many bytes, after redaction (cutting first
/// could split a secret in two and defeat the match).
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Request bodies relayed from a follower to the leader.
const MAX_RELAYED_BODY: usize = 5 * 1024 * 1024;
/// rmcp's own idle timeout is minutes; ours for focus is a day, and a session
/// that dies sooner would orphan the focus the AI set up.
const SESSION_IDLE: Duration = Duration::from_secs(24 * 3600);

const INSTRUCTIONS: &str = "\
Clouduler cluster operator. Work on ONE stack at a time.

Workflow:
1. focus_stack(name) - pick the stack you are working on (it may not exist yet). Writes, exec and \
read_file only work on the focused stack; reports (list_problems, cluster_overview) describe it in \
detail and only count problems elsewhere.
2. Read the docs resource clouduler://docs/stacks (also networking, secrets, proxy, and the \
clouduler://examples/* stacks) and call get_cluster_config so the networks, drives and domains you \
use are real.
3. validate_stack(spec) until it is valid, then deploy_stack(spec, configs). Put file contents in \
configs (put_config also works); never inline a secret.
4. After deploying check list_problems, then get_logs for any instance that is not healthy. Use exec \
and read_file to debug inside containers.

Secrets: NEVER ask the user to paste a secret and never try to read one. Create it with \
generate_secret (you only get its length and character facts), reference it from the stack's \
`secrets:` list, and verify its shape with inspect_secret (for example, no trailing newline). \
Rotating (overwrite) a secret a running service already uses can break it.

All output is redacted: values of cluster secrets appear as [REDACTED secret:NAME]. If you see that \
marker, that is expected, not a bug.";

/// Everything `/mcp` keeps in memory: the protocol service and its sessions.
pub struct McpHub {
    service: StreamableHttpService<McpServer, LocalSessionManager>,
}

impl Default for McpHub {
    fn default() -> Self {
        let sessions = Arc::new(Sessions::default());
        let mut manager = LocalSessionManager::default();
        manager.session_config.keep_alive = Some(SESSION_IDLE);
        // Host header validation guards a locally running server against DNS
        // rebinding; this one is public by design and authenticates every
        // request with a bearer token instead.
        let config = StreamableHttpServerConfig::default().disable_allowed_hosts();
        let service = StreamableHttpService::new(
            move || {
                Ok(McpServer {
                    sessions: sessions.clone(),
                })
            },
            Arc::new(manager),
            config,
        );
        Self { service }
    }
}

pub fn router() -> Router<AppState> {
    Router::new().route("/mcp", any(endpoint))
}

/// Authenticate, then hand the request to rmcp with the caller and the app
/// state attached for the handler to find.
async fn endpoint(State(state): State<AppState>, mut request: Request) -> Response {
    let caller = match authenticate_mcp(request.headers(), &state).await {
        Ok(caller) => caller,
        Err(e) => return challenge(&state, e).await,
    };
    request.extensions_mut().insert(caller);
    request.extensions_mut().insert(state.clone());
    state.mcp.service.handle(request).await.map(Body::new)
}

/// Relay an `/mcp` request from a follower to the leader.
///
/// An MCP client reaches whichever node the dashboard hostname resolves to and
/// will not follow a 503 + Location. Sessions and focus live in the serving
/// node's memory, so they must all land on one node, the leader. The
/// response is streamed back because `/mcp` answers with server-sent events.
pub(crate) async fn forward_to_leader(state: &AppState, request: Request) -> Response {
    let Some(leader) = crate::server::leader_location(state).await else {
        return ApiError::NotLeader { leader: None }.into_response();
    };
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_RELAYED_BODY).await else {
        return ApiError::BadRequest("request body too large".into()).into_response();
    };
    let url = format!("{}{}", leader.trim_end_matches('/'), parts.uri.path());
    let mut req = state.http.request(parts.method.clone(), url).body(bytes);
    for name in [
        header::AUTHORIZATION,
        header::CONTENT_TYPE,
        header::ACCEPT,
        HeaderName::from_static("mcp-session-id"),
        HeaderName::from_static("mcp-protocol-version"),
        HeaderName::from_static("last-event-id"),
    ] {
        if let Some(v) = parts.headers.get(&name) {
            req = req.header(name, v);
        }
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

// ---- the rmcp handler ----

#[derive(Clone)]
pub struct McpServer {
    sessions: Arc<Sessions>,
}

/// What the HTTP layer attached to this request.
struct RequestInfo {
    state: AppState,
    caller: McpCaller,
    session: String,
    owner: String,
}

fn request_info(ctx: &RequestContext<RoleServer>) -> Result<RequestInfo, McpError> {
    let parts = ctx.extensions.get::<Parts>();
    let state = parts.and_then(|p| p.extensions.get::<AppState>());
    let caller = parts.and_then(|p| p.extensions.get::<McpCaller>());
    let (Some(parts), Some(state), Some(caller)) = (parts, state, caller) else {
        return Err(McpError::internal_error(
            "request arrived without authentication context",
            None,
        ));
    };
    let owner = format!(
        "{}|{}",
        caller.identity.subject,
        caller.client.as_deref().unwrap_or("-")
    );
    // A protocol version without sessions sends no id; focus then belongs to
    // the caller rather than to a conversation.
    let session = parts
        .headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| format!("stateless:{owner}"));
    Ok(RequestInfo {
        state: state.clone(),
        caller: caller.clone(),
        session,
        owner,
    })
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let info = request_info(&ctx)?;
        let admin = info.caller.grant == McpGrant::Admin;
        Ok(ListToolsResult::with_all_items(
            tools::catalog()
                .iter()
                .filter(|t| admin || !t.admin)
                .map(|t| t.tool.clone())
                .collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let info = request_info(&ctx)?;
        let name = request.name.to_string();
        if tools::is_admin_tool(&name).is_none() {
            return Err(McpError::invalid_params(
                format!("unknown tool {name:?}"),
                None,
            ));
        }
        let args = request.arguments.unwrap_or_default();
        let target = tools::audit_target(&name, &args);

        // Fail closed: without the redactor there is no telling whether output
        // holds a secret. Only the tools that echo nothing but the focus name
        // may run without one.
        let redactor = match build_redactor(&info.state).await {
            Ok(r) => Arc::new(r),
            Err(msg) if tools::is_focus_only(&name) => {
                let _ = msg;
                Arc::new(Redactor::from_plaintexts(Vec::new()).expect("empty redactor"))
            }
            Err(msg) => return Ok(CallToolResponse::from(text_result(msg, true))),
        };

        tracing::info!(
            subject = %info.caller.identity.subject,
            client = info.caller.client.as_deref().unwrap_or("api-key"),
            tool = %name,
            target = %redactor.redact(&target),
            "mcp tool call"
        );

        let tool_ctx = Ctx {
            state: info.state,
            caller: info.caller,
            session: info.session,
            owner: info.owner,
            sessions: self.sessions.clone(),
            redactor: redactor.clone(),
        };
        let outcome = tools::call(&tool_ctx, &name, args).await;
        let (text, is_error) = match outcome {
            Ok(t) => (t, false),
            Err(e) => (e, true),
        };
        Ok(CallToolResponse::from(text_result(
            finish(&redactor, text),
            is_error,
        )))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        request_info(&ctx)?;
        Ok(ListResourcesResult::with_all_items(
            resources::DOCS
                .iter()
                .map(|d| {
                    Resource::new(d.uri, d.name)
                        .with_description(d.description)
                        .with_mime_type(d.mime)
                })
                .collect(),
        ))
    }

    /// Docs and examples are compiled into the binary and cannot contain a
    /// cluster secret, so they skip the redactor on purpose: redacting them
    /// could only garble the text (a secret that happens to be a common word).
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        request_info(&ctx)?;
        let doc = resources::find(&request.uri).ok_or_else(|| {
            McpError::invalid_params(format!("no resource {}", request.uri), None)
        })?;
        Ok(ReadResourceResponse::from(ReadResourceResult::new(vec![
            ResourceContents::text(doc.text, doc.uri).with_mime_type(doc.mime),
        ])))
    }
}

/// A fresh redactor from the live state and key: rebuilt per call so a secret
/// rotated a second ago is already covered.
async fn build_redactor(state: &AppState) -> Result<Redactor, String> {
    let key = state.key.read().await.clone().ok_or_else(|| {
        "this node holds no cluster key, so output cannot be checked for secret values; refusing \
         to return it"
            .to_string()
    })?;
    let cluster_state = state.cluster.state().await;
    Redactor::from_state(&cluster_state, &key)
        .map_err(|e| format!("could not build the redactor: {e}"))
}

/// The single choke point every tool result passes through: redact, then
/// bound the size, then warn about what redaction could not cover.
fn finish(redactor: &Redactor, text: String) -> String {
    let mut text = redactor.redact(&text);
    if text.len() > MAX_OUTPUT_BYTES {
        let mut end = MAX_OUTPUT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[output truncated]");
    }
    if !redactor.unredactable.is_empty() {
        text.push_str(&format!(
            "\n\nnote: these secrets could not be decrypted on this node, so their values may \
             appear above unredacted: {}",
            redactor.unredactable.join(", ")
        ));
    }
    if !redactor.too_short.is_empty() {
        text.push_str(&format!(
            "\n\nnote: these secrets are too short to redact safely, so their values may appear \
             above: {}",
            redactor.too_short.join(", ")
        ));
    }
    text
}

fn text_result(text: String, is_error: bool) -> CallToolResult {
    let content = vec![ContentBlock::text(text)];
    if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redactor() -> Redactor {
        Redactor::from_plaintexts(vec![
            ("token".into(), b"s3cr3t-token-value".to_vec()),
            ("pin".into(), b"123".to_vec()),
        ])
        .unwrap()
    }

    #[test]
    fn finish_redacts_notes_what_it_cannot_cover_and_bounds_output() {
        let out = finish(&redactor(), "auth: s3cr3t-token-value ok".into());
        assert!(!out.contains("s3cr3t-token-value"));
        assert!(out.contains("[REDACTED secret:token]"));
        assert!(out.contains("too short to redact safely"));
        assert!(out.contains("pin"));

        let big = "x".repeat(MAX_OUTPUT_BYTES + 100);
        let out = finish(&redactor(), big);
        assert!(out.contains("[output truncated]"));
        assert!(out.len() < MAX_OUTPUT_BYTES + 400);
    }

    #[test]
    fn a_secret_cannot_survive_by_being_split_at_the_size_cap() {
        let mut text = "a".repeat(MAX_OUTPUT_BYTES - 5);
        text.push_str("s3cr3t-token-value");
        let out = finish(&redactor(), text);
        assert!(!out.contains("s3cr3t"));
    }
}
