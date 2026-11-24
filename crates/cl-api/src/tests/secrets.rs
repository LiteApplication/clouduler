use super::*;

/// A registry login is an ordinary sealed secret under a reserved name, so
/// the generic secret listing has to hide it exactly like the Cloudflare
/// token — otherwise it shows up as a mystery entry an operator can delete
/// out from under every pull in the cluster.
#[tokio::test]
async fn registry_logins_do_not_show_up_among_the_users_secrets() {
    let h = harness().await;
    let name = cl_sched::registries::secret_name("docker.io");
    h.state
        .cluster
        .submit(cl_raft::Request::PutSecret {
            name: name.clone(),
            sealed: "AAAA".into(),
            key_fingerprint: "test".into(),
            owner: None,
        })
        .await
        .expect("store");

    let resp = h
        .router
        .oneshot(
            Request::builder()
                .uri("/api/secrets")
                .header("authorization", format!("Bearer {}", h.admin_key))
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
    let listed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let names: Vec<&str> = listed
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        !names.contains(&name.as_str()),
        "{name} leaked into {names:?}"
    );
}

fn wrap_request(token: Option<&str>, public_key: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/secrets/key/wrap")
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder
        .body(Body::from(
            serde_json::json!({"node": "n2", "public_key": public_key}).to_string(),
        ))
        .unwrap()
}

#[tokio::test]
async fn the_wrapped_cluster_key_opens_only_with_the_joining_nodes_keypair() {
    let h = harness().await;
    let held = h
        .state
        .key
        .read()
        .await
        .clone()
        .expect("harness holds a key");
    let keypair = cl_secret::NodeKeypair::generate();

    let resp = h
        .router
        .clone()
        .oneshot(wrap_request(
            Some(&h.admin_key),
            &keypair.public_key_encoded(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(
        !text.contains(&held.encode()),
        "the raw key must not be returned"
    );

    let body: crate::routes::key_distribution::WrapResponse = serde_json::from_str(&text).unwrap();
    assert_eq!(body.key_fingerprint, held.fingerprint());
    let wrapped = cl_secret::WrappedKey {
        ephemeral_public: body.ephemeral_public,
        sealed: cl_secret::Sealed::from_blob(body.sealed),
    };
    let opened = wrapped.unwrap_with(&keypair, "n2").expect("opens");
    assert_eq!(opened.encode(), held.encode());
    assert!(wrapped
        .unwrap_with(&cl_secret::NodeKeypair::generate(), "n2")
        .is_err());
    assert!(wrapped.unwrap_with(&keypair, "n3").is_err());
}

#[tokio::test]
async fn wrapping_the_cluster_key_needs_an_admin() {
    let h = harness().await;
    let public = cl_secret::NodeKeypair::generate().public_key_encoded();

    let anonymous = h
        .router
        .clone()
        .oneshot(wrap_request(None, &public))
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let cookie = viewer_cookie(&h.state).await;
    let viewer = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/secrets/key/wrap")
                .header("cookie", cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"node": "n2", "public_key": public}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(viewer.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_node_without_the_key_refuses_to_wrap_one() {
    let h = harness().await;
    *h.state.key.write().await = None;
    let public = cl_secret::NodeKeypair::generate().public_key_encoded();

    let resp = h
        .router
        .clone()
        .oneshot(wrap_request(Some(&h.admin_key), &public))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    let resp = h
        .router
        .clone()
        .oneshot(wrap_request(Some(&h.admin_key), "not-a-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
