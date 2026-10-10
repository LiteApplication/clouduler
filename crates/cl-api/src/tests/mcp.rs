//! The MCP endpoint end to end, driven over HTTP with `oneshot`: initialize,
//! list tools, call tools. Docker is not available here, so container tools
//! are exercised at the refusal and scoping level.

use super::oauth_flow::{
    admin, approve, authorize, call, code_exchange, code_of, register, VERIFIER,
};
use super::*;
use http_body_util::BodyExt;
use serde_json::{json, Value};

const SECRET: &str = "hunter2-super-secret-value";

/// One MCP session: a bearer token plus the `Mcp-Session-Id` the server
/// issued on `initialize`.
struct Mcp<'a> {
    h: &'a Harness,
    bearer: String,
    session: Option<String>,
    next_id: u64,
}

/// A JSON-RPC response may arrive as plain JSON or inside an SSE stream.
fn parse_body(content_type: &str, body: &str) -> Value {
    if content_type.contains("event-stream") {
        body.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
            .next_back()
            .unwrap_or(Value::Null)
    } else {
        serde_json::from_str(body).unwrap_or(Value::Null)
    }
}

async fn post_raw(
    h: &Harness,
    bearer: Option<&str>,
    session: Option<&str>,
    body: Value,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "cloud")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(b) = bearer {
        req = req.header("authorization", format!("Bearer {b}"));
    }
    if let Some(s) = session {
        req = req.header("mcp-session-id", s);
    }
    let resp = h
        .router
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = parse_body(&content_type, &String::from_utf8_lossy(&bytes));
    if json.is_null() && !bytes.is_empty() {
        eprintln!("raw {status}: {}", String::from_utf8_lossy(&bytes));
    }
    (status, headers, json)
}

impl<'a> Mcp<'a> {
    async fn connect(h: &'a Harness, bearer: &str) -> Mcp<'a> {
        let (status, headers, body) = post_raw(
            h,
            Some(bearer),
            None,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1"}
            }}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("focus_stack"));
        let session = headers
            .get("mcp-session-id")
            .map(|v| v.to_str().unwrap().to_string());
        let (status, _, _) = post_raw(
            h,
            Some(bearer),
            session.as_deref(),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        Mcp {
            h,
            bearer: bearer.to_string(),
            session,
            next_id: 2,
        }
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let (status, _, body) = post_raw(
            self.h,
            Some(&self.bearer),
            self.session.as_deref(),
            json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{method}: {body}");
        body
    }

    async fn tool_names(&mut self) -> Vec<String> {
        let body = self.rpc("tools/list", json!({})).await;
        body["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{body}"))
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// Calls a tool and returns `(is_error, text)`.
    async fn tool(&mut self, name: &str, args: Value) -> (bool, String) {
        let body = self
            .rpc("tools/call", json!({"name": name, "arguments": args}))
            .await;
        let result = &body["result"];
        assert!(result.is_object(), "{body}");
        (
            result["isError"].as_bool().unwrap_or(false),
            result["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_string(),
        )
    }
}

async fn read_token(h: &Harness) -> String {
    let client_id = register(h).await;
    let req = authorize(h, &client_id).await;
    let (_, body) = approve(h, &admin(h), &req, "read").await;
    let code = code_of(&body);
    let (status, tokens) = code_exchange(h, &client_id, &code, VERIFIER).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    tokens["access_token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn no_token_is_a_401_with_a_challenge() {
    let h = harness().await;
    let (status, headers, _) = post_raw(&h, None, None, json!({})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let challenge = headers.get("www-authenticate").unwrap().to_str().unwrap();
    assert!(challenge.contains("resource_metadata="), "{challenge}");

    let (status, _, _) = post_raw(&h, Some("clk_nope"), None, json!({})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_disabled_mcp_is_a_404() {
    let h = harness().await;
    h.state.cluster_config.write().await.mcp.enabled = false;
    let (status, _, _) = post_raw(&h, Some(&h.admin_key), None, json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_admin_sees_the_full_tool_list_and_a_reader_only_the_read_tools() {
    let h = harness().await;
    let mut admin_session = Mcp::connect(&h, &h.admin_key).await;
    let admin_tools = admin_session.tool_names().await;
    for t in [
        "focus_stack",
        "get_focus",
        "clear_focus",
        "cluster_overview",
        "list_problems",
        "list_stacks",
        "get_stack",
        "list_instances",
        "get_instance",
        "get_logs",
        "list_nodes",
        "get_cluster_config",
        "list_configs",
        "get_config",
        "list_secrets",
        "inspect_secret",
        "validate_stack",
        "deploy_stack",
        "remove_stack",
        "restart_instance",
        "update_instance",
        "put_config",
        "generate_secret",
        "freeze_volume",
        "unfreeze_volume",
        "exec",
        "read_file",
    ] {
        assert!(admin_tools.contains(&t.to_string()), "missing {t}");
    }
    assert_eq!(admin_tools.len(), 27, "{admin_tools:?}");

    let token = read_token(&h).await;
    let mut reader = Mcp::connect(&h, &token).await;
    let tools = reader.tool_names().await;
    assert!(tools.contains(&"list_problems".to_string()));
    for t in [
        "exec",
        "deploy_stack",
        "generate_secret",
        "read_file",
        "put_config",
        "freeze_volume",
        "unfreeze_volume",
    ] {
        assert!(!tools.contains(&t.to_string()), "{t} listed for a reader");
    }

    // Unlisted is not the only guard: calling it is refused too.
    let (is_error, text) = reader
        .tool("generate_secret", json!({"name": "x", "length": 8}))
        .await;
    assert!(is_error);
    assert!(text.contains("admin"), "{text}");
}

#[tokio::test]
async fn focus_is_remembered_per_session() {
    let h = harness().await;
    let mut a = Mcp::connect(&h, &h.admin_key).await;
    assert_eq!(
        a.tool("get_focus", json!({})).await.1,
        "no stack is focused"
    );
    let (is_error, text) = a.tool("focus_stack", json!({"name": "blog"})).await;
    assert!(!is_error, "{text}");
    assert!(text.contains("does not exist yet"), "{text}");
    assert!(a.tool("get_focus", json!({})).await.1.contains("blog"));

    // Another session of the same caller starts unfocused.
    let mut b = Mcp::connect(&h, &h.admin_key).await;
    assert_eq!(
        b.tool("get_focus", json!({})).await.1,
        "no stack is focused"
    );

    let (_, text) = a.tool("list_problems", json!({})).await;
    assert!(text.contains("stack blog: no problems"), "{text}");
    a.tool("clear_focus", json!({})).await;
    assert_eq!(
        a.tool("get_focus", json!({})).await.1,
        "no stack is focused"
    );
}

#[tokio::test]
async fn writes_and_exec_are_refused_without_focus_and_outside_it() {
    let h = harness().await;
    let mut m = Mcp::connect(&h, &h.admin_key).await;

    let spec = "services:\n  web:\n    image: nginx\n";
    for (tool, args) in [
        ("deploy_stack", json!({"spec": spec})),
        ("exec", json!({"target": "blog.web.0", "cmd": ["ls"]})),
        (
            "read_file",
            json!({"target": "blog.web.0", "path": "/etc/hostname"}),
        ),
        ("remove_stack", json!({"confirm": "blog"})),
        ("restart_instance", json!({"id": "blog.web.0"})),
        ("put_config", json!({"name": "c", "content": "x"})),
        ("generate_secret", json!({"name": "s", "length": 8})),
    ] {
        let (is_error, text) = m.tool(tool, args).await;
        assert!(is_error, "{tool} should be refused without focus");
        assert!(text.contains("focus_stack"), "{tool}: {text}");
    }

    m.tool("focus_stack", json!({"name": "mine"})).await;
    for (tool, args) in [
        ("exec", json!({"target": "other.web.0", "cmd": ["ls"]})),
        (
            "read_file",
            json!({"target": "other.web.0", "path": "/etc/hostname"}),
        ),
        ("restart_instance", json!({"id": "other.web.0"})),
        ("update_instance", json!({"id": "other.web.0"})),
    ] {
        let (is_error, text) = m.tool(tool, args).await;
        assert!(is_error, "{tool} outside focus");
        assert!(text.contains("focused stack is mine"), "{tool}: {text}");
        assert!(text.contains("focus_stack"), "{tool}: {text}");
    }
    let (is_error, text) = m.tool("remove_stack", json!({"confirm": "other"})).await;
    assert!(is_error);
    assert!(text.contains("mine"), "{text}");
}

#[tokio::test]
async fn read_file_refuses_secret_mounts_however_they_are_spelled() {
    let h = harness().await;
    let mut m = Mcp::connect(&h, &h.admin_key).await;
    m.tool("focus_stack", json!({"name": "mine"})).await;
    for path in [
        "/run/secrets/db",
        "/run/secrets",
        "/run/./secrets/../secrets/db",
    ] {
        let (is_error, text) = m
            .tool("read_file", json!({"target": "mine.web.0", "path": path}))
            .await;
        assert!(is_error);
        assert!(text.contains("refusing to read"), "{path}: {text}");
    }
}

/// Stores a secret and a config embedding its value, the way an app that
/// logs its own password into a file would.
async fn plant_secret(h: &Harness) {
    let auth = admin(h);
    let (status, _, body) = call(
        h,
        "PUT",
        "/api/secrets/dbpass",
        &super::oauth_flow::json_headers(auth.clone()),
        Body::from(json!({"value": SECRET}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let content = cl_raft::base64_encode(format!("password={SECRET}\nmode=prod\n").as_bytes());
    let (status, _, body) = call(
        h,
        "PUT",
        "/api/configs/app-conf",
        &super::oauth_flow::json_headers(auth),
        Body::from(json!({"content": content}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_secret_embedded_in_a_config_is_redacted() {
    let h = harness().await;
    plant_secret(&h).await;
    let mut m = Mcp::connect(&h, &h.admin_key).await;

    let (is_error, text) = m.tool("get_config", json!({"name": "app-conf"})).await;
    assert!(!is_error, "{text}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(text.contains("[REDACTED secret:dbpass]"), "{text}");
    assert!(text.contains("mode=prod"));

    // Errors are redacted too, and nothing lists a value.
    let (_, text) = m.tool("list_secrets", json!({})).await;
    assert!(text.contains("dbpass") && !text.contains(SECRET));

    // The copy of a secret into a new config is refused outright.
    m.tool("focus_stack", json!({"name": "app"})).await;
    let (is_error, text) = m
        .tool(
            "put_config",
            json!({"name": "leak", "content": format!("p={SECRET}")}),
        )
        .await;
    assert!(is_error);
    assert!(!text.contains(SECRET));
}

#[tokio::test]
async fn deploy_refuses_a_spec_with_a_secrets_plaintext() {
    let h = harness().await;
    plant_secret(&h).await;
    let mut m = Mcp::connect(&h, &h.admin_key).await;
    m.tool("focus_stack", json!({"name": "app"})).await;
    let spec =
        format!("services:\n  web:\n    image: nginx\n    environment:\n      PW: {SECRET}\n");
    let (is_error, text) = m.tool("deploy_stack", json!({"spec": spec})).await;
    assert!(is_error);
    assert!(text.contains("dbpass"), "{text}");
    assert!(!text.contains(SECRET), "{text}");
}

#[tokio::test]
async fn a_generated_secret_reports_facts_but_never_its_value() {
    let h = harness().await;
    let mut m = Mcp::connect(&h, &h.admin_key).await;
    m.tool("focus_stack", json!({"name": "app"})).await;

    let (is_error, text) = m
        .tool(
            "generate_secret",
            json!({"name": "dbpw", "length": 32, "symbols": "-_"}),
        )
        .await;
    assert!(!is_error, "{text}");
    let facts: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(facts["facts"]["byte_len"], 32);

    let (is_error, text) = m.tool("inspect_secret", json!({"name": "dbpw"})).await;
    assert!(!is_error, "{text}");
    let report: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["facts"]["byte_len"], 32);
    assert_eq!(report["facts"]["char_len"], 32);
    assert_eq!(report["facts"]["trailing_newline"], false);

    // The value itself appears in neither answer.
    let state = h.state.cluster.state().await;
    let key = h.state.key.read().await.clone().unwrap();
    let value = cl_secret::Sealed::from_blob(state.secrets["dbpw"].sealed.clone())
        .open(&key, "dbpw")
        .unwrap();
    let value = String::from_utf8(value).unwrap();
    assert_eq!(value.len(), 32);
    assert!(!text.contains(&value));

    // A second generation does not silently rotate it.
    let (is_error, text) = m
        .tool("generate_secret", json!({"name": "dbpw", "length": 32}))
        .await;
    assert!(is_error);
    assert!(text.contains("overwrite"), "{text}");
    let (is_error, _) = m
        .tool(
            "generate_secret",
            json!({"name": "dbpw", "length": 16, "overwrite": true}),
        )
        .await;
    assert!(!is_error);
    let (_, text) = m.tool("inspect_secret", json!({"name": "dbpw"})).await;
    assert!(text.contains("\"byte_len\": 16"), "{text}");

    // Reserved names are not reachable.
    let (is_error, text) = m
        .tool(
            "generate_secret",
            json!({"name": crate::oidc::OIDC_CLIENT_SECRET, "length": 16, "overwrite": true}),
        )
        .await;
    assert!(is_error);
    assert!(text.contains("reserved"), "{text}");
}

#[tokio::test]
async fn validate_and_cluster_reads_work_and_resources_are_served() {
    let h = harness().await;
    let mut m = Mcp::connect(&h, &h.admin_key).await;
    m.tool("focus_stack", json!({"name": "web"})).await;

    let (is_error, text) = m
        .tool(
            "validate_stack",
            json!({"spec": "services:\n  web:\n    image: nginx\n"}),
        )
        .await;
    assert!(!is_error, "{text}");
    assert!(text.contains("\"valid\": true"), "{text}");
    let (_, text) = m
        .tool("validate_stack", json!({"spec": "services: [oops"}))
        .await;
    assert!(text.contains("\"valid\": false"), "{text}");

    let (_, text) = m.tool("get_cluster_config", json!({})).await;
    assert!(text.contains("networks"), "{text}");
    let (_, text) = m.tool("cluster_overview", json!({})).await;
    assert!(text.contains("quorum=yes"), "{text}");
    let (_, text) = m.tool("list_nodes", json!({})).await;
    assert!(text.contains("n0"), "{text}");

    let listed = m.rpc("resources/list", json!({})).await;
    let uris: Vec<&str> = listed["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    for u in [
        "clouduler://docs/stacks",
        "clouduler://docs/networking",
        "clouduler://docs/secrets",
        "clouduler://docs/proxy",
        "clouduler://examples/simple.yaml",
        "clouduler://examples/gitlab.yaml",
    ] {
        assert!(uris.contains(&u), "{u}");
    }
    let read = m
        .rpc("resources/read", json!({"uri": "clouduler://docs/stacks"}))
        .await;
    assert!(!read["result"]["contents"][0]["text"]
        .as_str()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn revoking_the_client_ends_access_mid_session() {
    let h = harness().await;
    let client_id = register(&h).await;
    let req = authorize(&h, &client_id).await;
    let (_, body) = approve(&h, &admin(&h), &req, "read").await;
    let (_, tokens) = code_exchange(&h, &client_id, &code_of(&body), VERIFIER).await;
    let token = tokens["access_token"].as_str().unwrap().to_string();

    let mut m = Mcp::connect(&h, &token).await;
    assert!(!m.tool_names().await.is_empty());
    let (status, _, _) = call(
        &h,
        "DELETE",
        &format!("/api/mcp/clients/{client_id}"),
        &admin(&h),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, headers, _) = post_raw(
        &h,
        Some(&token),
        m.session.as_deref(),
        json!({"jsonrpc": "2.0", "id": 9, "method": "tools/list"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(headers.contains_key("www-authenticate"));
}
