//! The MCP oauth flow end to end, driven through the router.

use super::*;
use crate::auth::oauth::{self, pkce_challenge, McpGrant};
use http_body_util::BodyExt;

const REDIRECT: &str = "http://localhost:4567/cb";
pub(super) const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

pub(super) type Headers = Vec<(&'static str, String)>;

pub(super) async fn call(
    h: &Harness,
    method: &str,
    uri: &str,
    headers: &Headers,
    body: Body,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let resp = h
        .router
        .clone()
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, headers, json)
}

pub(super) fn admin(h: &Harness) -> Headers {
    vec![("authorization", format!("Bearer {}", h.admin_key))]
}

pub(super) fn json_headers(mut base: Headers) -> Headers {
    base.push(("content-type", "application/json".into()));
    base
}

pub(super) async fn register(h: &Harness) -> String {
    let (status, _, body) = call(
        h,
        "POST",
        "/oauth/register",
        &json_headers(vec![]),
        Body::from(
            serde_json::json!({"client_name": "Test Client", "redirect_uris": [REDIRECT]})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["client_id"].as_str().unwrap().to_string()
}

/// Run /oauth/authorize and return the pending request id.
pub(super) async fn authorize(h: &Harness, client_id: &str) -> String {
    let query = serde_urlencoded::to_string([
        ("client_id", client_id),
        ("redirect_uri", REDIRECT),
        ("response_type", "code"),
        ("code_challenge", &pkce_challenge(VERIFIER)),
        ("code_challenge_method", "S256"),
        ("state", "xyz"),
        ("scope", "mcp:read"),
        ("resource", "https://cloud/mcp"),
    ])
    .unwrap();
    let (status, headers, _) = call(
        h,
        "GET",
        &format!("/oauth/authorize?{query}"),
        &vec![],
        Body::empty(),
    )
    .await;
    assert!(status.is_redirection(), "{status}");
    let location = headers.get("location").unwrap().to_str().unwrap();
    location
        .strip_prefix("/oauth/consent?req=")
        .unwrap_or_else(|| panic!("unexpected location {location}"))
        .to_string()
}

pub(super) async fn approve(
    h: &Harness,
    auth: &Headers,
    req: &str,
    grant: &str,
) -> (StatusCode, serde_json::Value) {
    let (status, _, body) = call(
        h,
        "POST",
        &format!("/api/oauth/requests/{req}/approve"),
        &json_headers(auth.clone()),
        Body::from(serde_json::json!({"grant": grant}).to_string()),
    )
    .await;
    (status, body)
}

pub(super) fn code_of(redirect: &serde_json::Value) -> String {
    let uri = redirect["redirect"].as_str().unwrap();
    assert!(uri.starts_with(REDIRECT), "{uri}");
    assert!(uri.contains("state=xyz"));
    uri.split(['?', '&'])
        .find_map(|p| p.strip_prefix("code="))
        .unwrap()
        .to_string()
}

async fn token(h: &Harness, params: &[(&str, &str)]) -> (StatusCode, serde_json::Value) {
    let (status, _, body) = call(
        h,
        "POST",
        "/oauth/token",
        &vec![("content-type", "application/x-www-form-urlencoded".into())],
        Body::from(serde_urlencoded::to_string(params).unwrap()),
    )
    .await;
    (status, body)
}

pub(super) async fn code_exchange(
    h: &Harness,
    client_id: &str,
    code: &str,
    verifier: &str,
) -> (StatusCode, serde_json::Value) {
    token(
        h,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", client_id),
            ("redirect_uri", REDIRECT),
            ("code_verifier", verifier),
        ],
    )
    .await
}

async fn refresh(
    h: &Harness,
    client_id: &str,
    refresh_token: &str,
) -> (StatusCode, serde_json::Value) {
    token(
        h,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ],
    )
    .await
}

#[tokio::test]
async fn discovery_documents_describe_this_server() {
    let h = harness().await;
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let (status, _, body) = call(&h, "GET", path, &vec![], Body::empty()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["resource"], "https://cloud/mcp");
        assert_eq!(body["authorization_servers"][0], "https://cloud");
    }
    let (_, _, body) = call(
        &h,
        "GET",
        "/.well-known/oauth-authorization-server",
        &vec![],
        Body::empty(),
    )
    .await;
    assert_eq!(body["issuer"], "https://cloud");
    assert_eq!(body["token_endpoint"], "https://cloud/oauth/token");
    assert_eq!(body["code_challenge_methods_supported"][0], "S256");
}

#[tokio::test]
async fn registration_rejects_unsafe_redirect_uris() {
    let h = harness().await;
    let (status, _, body) = call(
        &h,
        "POST",
        "/oauth/register",
        &json_headers(vec![]),
        Body::from(serde_json::json!({"redirect_uris": ["http://evil.example/cb"]}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_redirect_uri");
}

#[tokio::test]
async fn authorize_never_redirects_to_an_unregistered_uri() {
    let h = harness().await;
    let client_id = register(&h).await;
    let query = serde_urlencoded::to_string([
        ("client_id", client_id.as_str()),
        ("redirect_uri", "http://localhost:1/evil"),
        ("response_type", "code"),
    ])
    .unwrap();
    let (status, _, _) = call(
        &h,
        "GET",
        &format!("/oauth/authorize?{query}"),
        &vec![],
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_full_flow_issues_rotates_and_revokes() {
    let h = harness().await;
    let client_id = register(&h).await;

    // The pending request is described to the consent page.
    let req = authorize(&h, &client_id).await;
    let (status, _, info) = call(
        &h,
        "GET",
        &format!("/api/oauth/requests/{req}"),
        &admin(&h),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["client_name"], "Test Client");
    assert_eq!(info["redirect_host"], "localhost:4567");
    assert_eq!(info["allowed_grants"], serde_json::json!(["read", "admin"]));

    // A wrong PKCE verifier is rejected, and burns the code.
    let (status, body) = approve(&h, &admin(&h), &req, "admin").await;
    assert_eq!(status, StatusCode::OK);
    let code = code_of(&body);
    let (status, body) = code_exchange(&h, &client_id, &code, "wrong-verifier").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");
    let (status, _) = code_exchange(&h, &client_id, &code, VERIFIER).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a burnt code stays burnt");

    // A fresh request, redeemed properly.
    let req = authorize(&h, &client_id).await;
    let (_, body) = approve(&h, &admin(&h), &req, "admin").await;
    let code = code_of(&body);
    let (status, tokens) = code_exchange(&h, &client_id, &code, VERIFIER).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["expires_in"], 3600);
    let access = tokens["access_token"].as_str().unwrap().to_string();
    let first_refresh = tokens["refresh_token"].as_str().unwrap().to_string();

    // Single use.
    let (status, _) = code_exchange(&h, &client_id, &code, VERIFIER).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let grant = oauth::verify_access_token(&h.state, &access).await.unwrap();
    assert_eq!(grant.grant, McpGrant::Admin);
    assert_eq!(grant.client_id, client_id);
    assert!(grant.identity.role.is_admin());

    let mut headers = axum::http::HeaderMap::new();
    headers.insert("authorization", format!("Bearer {access}").parse().unwrap());
    let caller = oauth::authenticate_mcp(&headers, &h.state).await.unwrap();
    assert_eq!(caller.client.as_deref(), Some(client_id.as_str()));

    // An api key is an admin grant with no client.
    headers.insert(
        "authorization",
        format!("Bearer {}", h.admin_key).parse().unwrap(),
    );
    let caller = oauth::authenticate_mcp(&headers, &h.state).await.unwrap();
    assert_eq!(caller.grant, McpGrant::Admin);
    assert!(caller.client.is_none());

    // Refresh rotates: the old token is dead.
    let (status, rotated) = refresh(&h, &client_id, &first_refresh).await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    let new_refresh = rotated["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(new_refresh, first_refresh);
    let (status, body) = refresh(&h, &client_id, &first_refresh).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");

    // The client is listed, and revoking it kills access immediately.
    let (status, _, list) = call(&h, "GET", "/api/mcp/clients", &admin(&h), Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list[0]["client_id"], client_id.as_str());
    assert_eq!(list[0]["grant"], "admin");

    let viewer = vec![("cookie", viewer_cookie(&h.state).await)];
    let delete_uri = format!("/api/mcp/clients/{client_id}");
    let (status, _, _) = call(&h, "DELETE", &delete_uri, &viewer, Body::empty()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = call(&h, "DELETE", &delete_uri, &admin(&h), Body::empty()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let new_access = rotated["access_token"].as_str().unwrap();
    assert!(oauth::verify_access_token(&h.state, &access).await.is_err());
    assert!(oauth::verify_access_token(&h.state, new_access)
        .await
        .is_err());
    let (status, _) = refresh(&h, &client_id, &new_refresh).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_viewer_cannot_approve_an_admin_grant() {
    let h = harness().await;
    let client_id = register(&h).await;
    let req = authorize(&h, &client_id).await;
    let viewer = vec![("cookie", viewer_cookie(&h.state).await)];

    let (_, _, info) = call(
        &h,
        "GET",
        &format!("/api/oauth/requests/{req}"),
        &viewer,
        Body::empty(),
    )
    .await;
    assert_eq!(info["allowed_grants"], serde_json::json!(["read"]));

    let (status, _) = approve(&h, &viewer, &req, "admin").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // The refusal did not consume the request.
    let (status, body) = approve(&h, &viewer, &req, "read").await;
    assert_eq!(status, StatusCode::OK);
    let (status, tokens) = code_exchange(&h, &client_id, &code_of(&body), VERIFIER).await;
    assert_eq!(status, StatusCode::OK);
    let grant = oauth::verify_access_token(&h.state, tokens["access_token"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(grant.grant, McpGrant::Read);
    assert!(!grant.identity.role.is_admin());
}

#[tokio::test]
async fn denying_redirects_with_access_denied() {
    let h = harness().await;
    let client_id = register(&h).await;
    let req = authorize(&h, &client_id).await;
    let (status, _, body) = call(
        &h,
        "POST",
        &format!("/api/oauth/requests/{req}/deny"),
        &admin(&h),
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let uri = body["redirect"].as_str().unwrap();
    assert!(uri.contains("error=access_denied") && uri.contains("state=xyz"));
}

#[tokio::test]
async fn disabling_mcp_hides_every_endpoint_but_the_client_list() {
    let h = harness().await;
    h.state.cluster_config.write().await.mcp.enabled = false;

    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-authorization-server",
        "/oauth/authorize?client_id=x",
        "/api/oauth/requests/abc",
    ] {
        let (status, _, _) = call(&h, "GET", path, &admin(&h), Body::empty()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    let (status, _) = token(&h, &[("grant_type", "refresh_token")]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = call(
        &h,
        "POST",
        "/oauth/register",
        &json_headers(vec![]),
        Body::from(serde_json::json!({"redirect_uris": [REDIRECT]}).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", h.admin_key).parse().unwrap(),
    );
    assert!(matches!(
        oauth::authenticate_mcp(&headers, &h.state).await,
        Err(crate::ApiError::NotFound(_))
    ));

    let (status, _, _) = call(&h, "GET", "/api/mcp/clients", &admin(&h), Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn an_unauthenticated_mcp_call_gets_a_resource_metadata_challenge() {
    let h = harness().await;
    let err = oauth::authenticate_mcp(&axum::http::HeaderMap::new(), &h.state)
        .await
        .unwrap_err();
    let resp = oauth::challenge(&h.state, err).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers().get("www-authenticate").unwrap(),
        "Bearer resource_metadata=\"https://cloud/.well-known/oauth-protected-resource\""
    );
}

#[tokio::test]
async fn auth_me_reports_whether_mcp_is_enabled() {
    let h = harness().await;
    let (_, _, me) = call(&h, "GET", "/auth/me", &admin(&h), Body::empty()).await;
    assert_eq!(me["mcp_enabled"], true);
    assert_eq!(me["role"], "admin");
}
