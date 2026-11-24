use super::*;

#[tokio::test]
async fn internal_instance_and_node_routes_reject_an_unsigned_request() {
    let h = harness().await;
    h.state
        .cluster
        .submit(cl_raft::Request::Assign {
            instance: "blog.web.0".parse().unwrap(),
            node: "n0".into(),
            drives: Default::default(),
        })
        .await
        .expect("assign");

    for path in [
        "/api/instances/internal/blog.web.0/summary",
        "/api/nodes/internal/n0/usage",
    ] {
        let resp = h
            .router
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{path} answered without a signature"
        );
    }
}


/// As [`internal_request`] but v3-only: the nonce header and v3 signature,
/// signed over `signed_nonce` while `nonce` is what is sent.
async fn v3_request(h: &Harness, path: &str, nonce: &str, signed_nonce: &str) -> Request<Body> {
    use cl_update::internal_client::*;
    let key_guard = h.state.key.read().await;
    let key = key_guard.as_ref().expect("test cluster holds a key");
    let ts = cl_secret::internal_auth::unix_now();
    let hash = cl_secret::internal_auth::body_sha256_hex(b"");
    let sig = cl_secret::internal_auth::sign_v3(key, "GET", path, ts, signed_nonce, &hash);
    Request::builder()
        .uri(path)
        .header(TIMESTAMP_HEADER, ts.to_string())
        .header(BODY_SHA256_HEADER, hash)
        .header(NONCE_HEADER, nonce)
        .header(SIGNATURE_V3_HEADER, sig)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn internal_v3_requests_differing_only_in_nonce_both_pass() {
    let h = harness().await;
    let path = "/api/nodes/internal/n0/usage";
    for _ in 0..2 {
        let n = cl_secret::internal_auth::new_nonce();
        let resp = h
            .router
            .clone()
            .oneshot(v3_request(&h, path, &n, &n).await)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

/// A request as the real client sends it: v2 and v3 signatures together.
/// `with_v3` false strips it down to the v2 headers an attacker could replay.
async fn dual_request(
    h: &Harness,
    path: &str,
    ts: u64,
    nonce: &str,
    with_v3: bool,
) -> Request<Body> {
    use cl_update::internal_client::*;
    let key_guard = h.state.key.read().await;
    let key = key_guard.as_ref().expect("test cluster holds a key");
    let hash = cl_secret::internal_auth::body_sha256_hex(b"");
    let mut b = Request::builder()
        .uri(path)
        .header(TIMESTAMP_HEADER, ts.to_string())
        .header(BODY_SHA256_HEADER, &hash)
        .header(
            SIGNATURE_HEADER,
            cl_secret::internal_auth::sign_v2(key, "GET", path, ts, &hash),
        );
    if with_v3 {
        b = b.header(NONCE_HEADER, nonce).header(
            SIGNATURE_V3_HEADER,
            cl_secret::internal_auth::sign_v3(key, "GET", path, ts, nonce, &hash),
        );
    }
    b.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn internal_v3_burns_the_v2_signature_so_stripping_v3_cannot_replay() {
    let h = harness().await;
    let path = "/api/nodes/internal/n0/usage";
    let ts = cl_secret::internal_auth::unix_now();
    let n = cl_secret::internal_auth::new_nonce();
    let resp = h
        .router
        .clone()
        .oneshot(dual_request(&h, path, ts, &n, true).await)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = h
        .router
        .clone()
        .oneshot(dual_request(&h, path, ts, &n, false).await)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn internal_v3_requests_sharing_a_v2_signature_both_pass() {
    let h = harness().await;
    let path = "/api/nodes/internal/n0/usage";
    let ts = cl_secret::internal_auth::unix_now();
    for _ in 0..2 {
        let n = cl_secret::internal_auth::new_nonce();
        let resp = h
            .router
            .clone()
            .oneshot(dual_request(&h, path, ts, &n, true).await)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn internal_v3_rejects_a_replayed_request() {
    let h = harness().await;
    let path = "/api/nodes/internal/n0/usage";
    let n = cl_secret::internal_auth::new_nonce();
    let ts = cl_secret::internal_auth::unix_now();
    let hash = cl_secret::internal_auth::body_sha256_hex(b"");
    let key_guard = h.state.key.read().await;
    let sig =
        cl_secret::internal_auth::sign_v3(key_guard.as_ref().unwrap(), "GET", path, ts, &n, &hash);
    drop(key_guard);
    for expected in [StatusCode::OK, StatusCode::UNAUTHORIZED] {
        let req = Request::builder()
            .uri(path)
            .header(cl_update::internal_client::TIMESTAMP_HEADER, ts.to_string())
            .header(cl_update::internal_client::BODY_SHA256_HEADER, &hash)
            .header(cl_update::internal_client::NONCE_HEADER, &n)
            .header(cl_update::internal_client::SIGNATURE_V3_HEADER, &sig)
            .body(Body::empty())
            .unwrap();
        let resp = h.router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), expected);
    }
}

#[tokio::test]
async fn internal_v3_rejects_a_signature_over_a_different_nonce() {
    let h = harness().await;
    let path = "/api/nodes/internal/n0/usage";
    let sent = cl_secret::internal_auth::new_nonce();
    let signed = cl_secret::internal_auth::new_nonce();
    let resp = h
        .router
        .clone()
        .oneshot(v3_request(&h, path, &sent, &signed).await)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn internal_routes_accept_a_v2_only_request_for_rolling_upgrades() {
    let h = harness().await;
    // `internal_request` sends no nonce or v3 signature, as an old client.
    let resp = signed_get(&h, "/api/nodes/internal/n0/usage").await;
    assert_eq!(resp.status(), StatusCode::OK);
}


#[tokio::test]
async fn internal_routes_reject_a_legacy_only_request() {
    let h = harness().await;
    let path = "/api/nodes/internal/n0/usage";
    let key_guard = h.state.key.read().await;
    let legacy = cl_secret::internal_auth::sign(key_guard.as_ref().unwrap(), path);
    drop(key_guard);
    let resp = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header(cl_update::internal_client::AUTH_HEADER, legacy)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn internal_routes_reject_a_replayed_signature() {
    let h = harness().await;
    let path = "/api/nodes/internal/n0/usage";
    let empty = cl_secret::internal_auth::body_sha256_hex(b"");
    let now = cl_secret::internal_auth::unix_now();
    for expected in [StatusCode::OK, StatusCode::UNAUTHORIZED] {
        let req = internal_request(&h, "GET", path, now, &empty, Vec::new()).await;
        let resp = h.router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), expected);
    }
}

#[tokio::test]
async fn internal_routes_reject_stale_and_future_timestamps() {
    let h = harness().await;
    let path = "/api/nodes/internal/n0/usage";
    let empty = cl_secret::internal_auth::body_sha256_hex(b"");
    let now = cl_secret::internal_auth::unix_now();
    for ts in [now - 120, now + 120] {
        let req = internal_request(&h, "GET", path, ts, &empty, Vec::new()).await;
        let resp = h.router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "ts {ts}");
    }
}

#[tokio::test]
async fn internal_routes_reject_a_signature_for_another_method() {
    let h = harness().await;
    let path = "/api/nodes/internal/n0/usage";
    let empty = cl_secret::internal_auth::body_sha256_hex(b"");
    let now = cl_secret::internal_auth::unix_now();
    let mut req = internal_request(&h, "POST", path, now, &empty, Vec::new()).await;
    *req.method_mut() = axum::http::Method::GET;
    let resp = h.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bodiless_internal_routes_reject_a_signed_nonempty_body_hash() {
    let h = harness().await;
    let path = "/api/nodes/internal/n0/usage";
    let hash = cl_secret::internal_auth::body_sha256_hex(b"x");
    let now = cl_secret::internal_auth::unix_now();
    let req = internal_request(&h, "GET", path, now, &hash, b"x".to_vec()).await;
    let resp = h.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn internal_apply_rejects_a_body_that_does_not_match_the_signed_hash() {
    let h = harness().await;
    let path = "/api/updates/internal/apply";
    let signed_body = br#"{"instances":["blog.web.0"]}"#;
    let hash = cl_secret::internal_auth::body_sha256_hex(signed_body);
    let now = cl_secret::internal_auth::unix_now();
    // A replayer swaps in an empty selection, which would mean "everything".
    let req = internal_request(
        &h,
        "POST",
        path,
        now,
        &hash,
        br#"{"instances":[]}"#.to_vec(),
    )
    .await;
    let resp = h.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let now = cl_secret::internal_auth::unix_now();
    let req = internal_request(&h, "POST", path, now + 1, &hash, signed_body.to_vec()).await;
    let resp = h.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn a_correctly_signed_internal_summary_request_answers_locally() {
    let h = harness().await;
    h.state
        .cluster
        .submit(cl_raft::Request::Assign {
            instance: "blog.web.0".parse().unwrap(),
            node: "n0".into(),
            drives: Default::default(),
        })
        .await
        .expect("assign");

    let resp = signed_get(&h, "/api/instances/internal/blog.web.0/summary").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["id"], "blog.web.0");
    assert_eq!(value["node"], "n0");
}

#[tokio::test]
async fn a_correctly_signed_internal_usage_request_answers_with_a_real_reading() {
    let h = harness().await;
    let resp = signed_get(&h, "/api/nodes/internal/n0/usage").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    // `local: true` is the signal a forwarding caller relies on to tell a
    // real reading from the zeroed fallback it uses when a peer cannot be
    // reached at all.
    assert_eq!(value["local"], true);
    assert!(value["cpu_count"].as_u64().unwrap() > 0);
}
