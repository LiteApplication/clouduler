use super::*;

/// A config that passes validation, built through `parse` so it stays valid as
/// the rules change rather than needing a field added here each time. The
/// harness's own config is deliberately minimal and does not validate, which is
/// fine for the routes that never check it but no good for the two tests below.
fn valid_config() -> ClusterConfig {
    ClusterConfig::parse(
        r#"
cluster:
  name: test
networks:
  internal:
    type: internal
    subnet: 10.10.0.0/24
    domain: home.test
nodes:
  n0:
    id: a6ce0990-6afe-4204-b235-82b1e133702e
    role: manager
    network:
      internal: 10.10.0.1
web:
  address: cloud
  network: internal
  port: 8443
"#,
    )
    .expect("the fixture config validates")
}

/// The ui posts the same `{"config": ...}` envelope it uses for PUT, and reads
/// `{valid, errors}` back. A bare config body used to be expected here, so the
/// call failed to deserialize before it ever reached validation.
#[tokio::test]
async fn validating_a_config_takes_the_same_envelope_as_applying_one() {
    let h = harness().await;
    let config = serde_json::to_string(&valid_config()).unwrap();

    let resp = h
        .router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/validate")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(format!(r#"{{"config":{config}}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(body["valid"], true);
    assert_eq!(body["errors"].as_array().unwrap().len(), 0);
}

/// An invalid config is an answer, not a transport failure: 200 with the
/// offending path, so the form can point at the field.
#[tokio::test]
async fn an_invalid_config_reports_the_field_that_broke() {
    let h = harness().await;
    let mut config = valid_config();
    config.data_dir = std::path::PathBuf::from("not/absolute");
    let config = serde_json::to_string(&config).unwrap();

    let resp = h
        .router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/validate")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(format!(r#"{{"config":{config}}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(body["valid"], false);
    assert_eq!(body["errors"][0]["path"], "data_dir");
}

/// The token is write-only: the status route says whether one exists, never
/// what it is.
#[tokio::test]
async fn the_cloudflare_token_status_never_carries_the_token() {
    let h = harness().await;
    let resp = h
        .router
        .oneshot(
            Request::builder()
                .uri("/api/cloudflare/token")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    // The harness config has no cloudflare section at all.
    assert_eq!(body["configured"], false);
    assert!(body.get("token").is_none());
}

/// Unlike the self-update repo, the token can be set more than once: it is
/// replicated as a cluster secret rather than written to a file on whichever
/// node answered the request, so rotating it is just overwriting that secret.
#[tokio::test]
async fn setting_the_cloudflare_token_can_be_rotated() {
    let h = harness().await;
    {
        let mut cfg = h.state.cluster_config.write().await;
        cfg.cloudflare = Some(cl_config::CloudflareConfig {
            zones: vec!["example.com".to_string()],
        });
    }

    let put = |token: &str| {
        h.router.clone().oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/cloudflare/token")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(format!(r#"{{"token":"{token}"}}"#)))
                .unwrap(),
        )
    };

    assert_eq!(
        put("cf-token-1").await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        put("cf-token-2").await.unwrap().status(),
        StatusCode::NO_CONTENT
    );

    let status = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/cloudflare/token")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(
        &http_body_util::BodyExt::collect(status.into_body())
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(body["configured"], true);
}

#[tokio::test]
async fn setting_a_cloudflare_token_without_cloudflare_enabled_still_stores_it() {
    let h = harness().await;
    let resp = h
        .router
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/cloudflare/token")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"token":"cf-token-123"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

/// The secret is write-only: the status route says whether one exists, never
/// what it is.
#[tokio::test]
async fn the_oidc_client_secret_status_never_carries_the_secret() {
    let h = harness().await;
    let resp = h
        .router
        .oneshot(
            Request::builder()
                .uri("/api/oidc/client-secret")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(body["configured"], false);
    assert!(body.get("client_secret").is_none());
}

/// Like the Cloudflare token, the secret can be set more than once: it is
/// replicated as a cluster secret rather than written to a file on whichever
/// node answered the request, so rotating it is just overwriting that secret.
#[tokio::test]
async fn setting_the_oidc_client_secret_can_be_rotated() {
    let h = harness().await;

    let put = |secret: &str| {
        h.router.clone().oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/oidc/client-secret")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(format!(r#"{{"client_secret":"{secret}"}}"#)))
                .unwrap(),
        )
    };

    assert_eq!(
        put("oidc-secret-1").await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        put("oidc-secret-2").await.unwrap().status(),
        StatusCode::NO_CONTENT
    );

    let status = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/oidc/client-secret")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(
        &http_body_util::BodyExt::collect(status.into_body())
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(body["configured"], true);
}

/// Setting the self-update GitHub token stores it as a sealed secret, and
/// the status route only ever reports whether one is configured.
#[tokio::test]
async fn setting_the_self_update_github_token_marks_it_configured() {
    let h = harness().await;
    let put = h.router.clone().oneshot(
        Request::builder()
            .method("PUT")
            .uri("/api/self-update/github-token")
            .header("authorization", format!("Bearer {}", h.admin_key))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"token":"ghp_test"}"#))
            .unwrap(),
    );
    assert_eq!(put.await.unwrap().status(), StatusCode::NO_CONTENT);

    let status = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/self-update/github-token")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &http_body_util::BodyExt::collect(status.into_body())
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(body["configured"], true);
}

/// The repo section is write-once: once set, `PUT` again is a conflict rather
/// than a silent overwrite.
#[tokio::test]
async fn setting_the_self_update_repo_twice_is_a_conflict() {
    let h = harness().await;
    let body = r#"{"release_url":"https://github.com/LiteApplication/clouduler"}"#;
    let put = || {
        h.router.clone().oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/self-update/repo")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
    };

    let first = put().await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);

    let second = put().await.unwrap();
    assert_eq!(second.status(), StatusCode::CONFLICT);
}

/// A run start with a malformed tag is rejected before anything is stored.
#[tokio::test]
async fn starting_a_self_update_run_rejects_a_bad_tag() {
    let h = harness().await;
    let response = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/self-update/run")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"tag":"a/b"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// The alert settings API never returns a destination's URL, and a client that
/// echoes the masked form back cannot overwrite the stored one.
#[tokio::test]
async fn alert_settings_mask_urls_and_a_masked_echo_keeps_the_secret() {
    use tower::ServiceExt;
    let h = harness().await;
    let call = |method: &'static str, uri: &'static str, body: String| {
        let router = h.router.clone();
        let key = h.admin_key.clone();
        async move {
            let resp = router
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("authorization", format!("Bearer {key}"))
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = resp.status();
            let bytes = http_body_util::BodyExt::collect(resp.into_body())
                .await
                .unwrap()
                .to_bytes();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_default(),
            )
        }
    };
    let dest = serde_json::json!({"name": "main", "kind": "webhook", "severities": ["critical"]});
    let url = "https://hooks.example.test/path/SECRETTOKEN";
    let (status, body) = call(
        "PUT",
        "/api/alerts/settings",
        serde_json::json!({"settings": {"destinations": [dest]}, "secrets": {"main": {"url": url, "auth": "Bearer AUTHVALUE"}}}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["secrets"]["main"]["url"],
        "https://hooks.example.test/…"
    );
    assert_eq!(body["secrets"]["main"]["auth_set"], true);
    let (_, got) = call("GET", "/api/alerts/settings", String::new()).await;
    let text = got.to_string();
    assert!(
        !text.contains("SECRETTOKEN") && !text.contains("AUTHVALUE"),
        "{text}"
    );

    // Echo the masked url back: accepted, and the stored one survives.
    let (status, _) = call(
        "PUT",
        "/api/alerts/settings",
        serde_json::json!({"settings": {"destinations": [dest]}, "secrets": {"main": {"url": "https://hooks.example.test/…"}}}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let st = h.state.cluster.state().await;
    let key = h.state.key.read().await.clone().unwrap();
    let stored =
        crate::alerts::send::open(&st, &key, &cl_alert::settings::url_secret("main")).unwrap();
    assert_eq!(stored.as_deref(), Some(url));

    // Removing the destination removes its secrets.
    let (status, _) = call(
        "PUT",
        "/api/alerts/settings",
        serde_json::json!({"settings": {}}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let st = h.state.cluster.state().await;
    assert!(!st
        .secrets
        .contains_key(&cl_alert::settings::url_secret("main")));
    assert!(!st
        .secrets
        .contains_key(&cl_alert::settings::auth_secret("main")));
}
