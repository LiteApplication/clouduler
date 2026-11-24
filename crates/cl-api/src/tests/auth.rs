use super::*;

#[tokio::test]
async fn unauthenticated_requests_are_rejected() {
    let h = harness().await;
    let resp = h
        .router
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_bad_api_key_fails() {
    let h = harness().await;
    let resp = h
        .router
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header("authorization", "Bearer clk_bogus_secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_good_api_key_reaches_the_handler() {
    let h = harness().await;
    let resp = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// The callback must reject a login this browser did not start before it
/// does anything else, and clear the state cookie either way.
#[tokio::test]
async fn an_oidc_callback_without_the_matching_state_cookie_is_rejected() {
    let h = harness().await;
    for cookie in [None, Some("cl_oidc_state=other")] {
        let mut req = Request::builder().uri("/auth/oidc/callback?code=c&state=abc");
        if let Some(cookie) = cookie {
            req = req.header("cookie", cookie);
        }
        let resp = h
            .router
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // Nothing to clear when the browser sent no state cookie.
        if cookie.is_some() {
            let cleared = resp
                .headers()
                .get("set-cookie")
                .expect("the state cookie is cleared")
                .to_str()
                .unwrap();
            assert!(cleared.starts_with("cl_oidc_state=;"), "got {cleared}");
        }
    }
}

/// The ui logs in with a key and then opens SSE streams, which `EventSource`
/// cannot put a bearer token on. So the key has to come back as a cookie the
/// browser will attach by itself.
#[tokio::test]
async fn logging_in_with_an_api_key_returns_a_session_cookie() {
    let h = harness().await;
    let resp = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(format!(r#"{{"key":"{}"}}"#, h.admin_key)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let cookie = resp
        .headers()
        .get("set-cookie")
        .expect("a session cookie is set")
        .to_str()
        .unwrap()
        .to_string();
    assert!(cookie.starts_with("cl_session="), "got {cookie}");
    assert!(cookie.contains("HttpOnly"), "got {cookie}");
    assert!(cookie.contains("Secure"), "got {cookie}");

    // That cookie alone, with no Authorization header, must reach the stream
    // endpoint that was returning "no credentials presented".
    let session = cookie.split(';').next().unwrap().to_string();
    let resp = h
        .router
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header("cookie", session)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn logging_in_with_a_bad_api_key_sets_no_cookie() {
    let h = harness().await;
    let resp = h
        .router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"key":"clk_bogus_secret"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().get("set-cookie").is_none());
}

#[tokio::test]
async fn a_viewer_cannot_write() {
    let h = harness().await;
    let cookie = viewer_cookie(&h.state).await;
    let resp = h
        .router
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/configs/nginx_conf")
                .header("cookie", cookie)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"content":"aGVsbG8="}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_viewer_never_receives_a_secret_value() {
    let h = harness().await;

    // Write the secret as an admin first.
    let put = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/secrets/db_password")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"value":"hunter2"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::OK);

    let cookie = viewer_cookie(&h.state).await;
    let resp = h
        .router
        .oneshot(
            Request::builder()
                .uri("/api/secrets")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(
        !text.contains("hunter2"),
        "a secret value must never be returned"
    );
}

#[tokio::test]
async fn a_session_is_stored_hashed_in_raft_and_logout_removes_it() {
    let h = harness().await;
    let identity = Identity {
        subject: "u".into(),
        username: "u".into(),
        role: Role::Admin,
    };
    let id = h
        .state
        .sessions
        .create(identity.clone(), Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(h.state.sessions.get(&id).await, Some(identity));

    let stored = h.state.cluster.state().await.sessions;
    assert_eq!(stored.len(), 1);
    assert!(!stored.contains_key(&id), "the raw id must not be stored");

    h.state.sessions.remove(&id).await;
    assert!(h.state.sessions.get(&id).await.is_none());
    assert!(h.state.cluster.state().await.sessions.is_empty());
}

#[tokio::test]
async fn an_expired_session_is_ignored_and_then_pruned() {
    let h = harness().await;
    let id = h
        .state
        .sessions
        .create(
            Identity {
                subject: "u".into(),
                username: "u".into(),
                role: Role::Viewer,
            },
            Duration::ZERO,
        )
        .await
        .unwrap();
    assert!(h.state.sessions.get(&id).await.is_none());
    h.state.sessions.prune_expired().await;
    assert!(h.state.cluster.state().await.sessions.is_empty());
}

/// Saved client addresses are listed and revoked by admins only, a delete
/// cannot reach an ordinary login session, and `/api/state` never shows them.
#[tokio::test]
async fn remembered_ips_are_admin_only_and_hidden_from_the_state() {
    use cl_raft::{ProxySessionRecord, Request as Raft};
    let h = harness().await;
    let far = i64::MAX / 4;
    let record = |host: &str| ProxySessionRecord {
        host: host.into(),
        subject: "sub".into(),
        username: "alice".into(),
        groups: vec!["family".into()],
        expires_at: far,
        parent: String::new(),
    };
    for (key, host) in [
        ("saved", "ip:public:203.0.113.7"),
        ("cookie", "app.example.com"),
    ] {
        let r = h
            .state
            .cluster
            .submit(Raft::PutProxySession {
                key: key.into(),
                session: record(host),
                now: 0,
            })
            .await
            .unwrap();
        assert!(r.error.is_none());
    }
    let call = |method: &str, uri: &str, auth: (&str, String)| {
        h.router.clone().oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(auth.0, auth.1)
                .body(Body::empty())
                .unwrap(),
        )
    };
    let admin = ("authorization", format!("Bearer {}", h.admin_key));

    let viewer = ("cookie", viewer_cookie(&h.state).await);
    assert_eq!(
        call("GET", "/api/proxy/remembered-ips", viewer)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );

    let listed = call("GET", "/api/proxy/remembered-ips", admin.clone())
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let rows: serde_json::Value = serde_json::from_slice(
        &http_body_util::BodyExt::collect(listed.into_body())
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["network"], "public");
    assert_eq!(rows[0]["ip"], "203.0.113.7");
    assert_eq!(rows[0]["username"], "alice");

    // The full state, as any signed-in viewer receives it, carries none of it.
    let state = call("GET", "/api/state", admin.clone()).await.unwrap();
    let text = String::from_utf8(
        http_body_util::BodyExt::collect(state.into_body())
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(!text.contains("203.0.113.7"), "{text}");

    // An ordinary login session is not a remembered address.
    assert_eq!(
        call("DELETE", "/api/proxy/remembered-ips/cookie", admin.clone())
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call("DELETE", "/api/proxy/remembered-ips/saved", admin.clone())
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    let after = h.state.cluster.state().await;
    assert!(!after.proxy_sessions.contains_key("saved"));
    assert!(after.proxy_sessions.contains_key("cookie"));
}
