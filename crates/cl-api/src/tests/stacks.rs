use super::*;



/// The UI deploy goes through the same cluster-aware check as the CLI: a
/// volume on a drive the cluster does not define is refused up front.
#[tokio::test]
async fn deploying_a_stack_that_names_an_unknown_drive_is_rejected() {
    let h = harness().await;
    let spec = "services:\n  web:\n    image: nginx\n    volumes:\n      - nodrive/vol:/data\n";
    let body = format!(r#"{{"spec":{spec:?}}}"#);
    assert_eq!(deploy(&h, "web", &body).await, StatusCode::BAD_REQUEST);
    assert!(!h.state.cluster.state().await.stacks.contains_key("web"));
}

/// Axum percent-decodes the path, so `a%2Fb` arrives as `a/b`, which the git
/// export would flatten onto stack `b`.
#[tokio::test]
async fn deploying_a_stack_with_an_unsafe_name_is_rejected() {
    let h = harness().await;
    assert_eq!(
        deploy(&h, "a%2Fb", PLAIN_SPEC).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(deploy(&h, "Web", PLAIN_SPEC).await, StatusCode::BAD_REQUEST);
    assert_eq!(deploy(&h, "web", PLAIN_SPEC).await, StatusCode::OK);
}

/// A stack that predates name validation must stay redeployable.
#[tokio::test]
async fn an_existing_stack_with_a_now_invalid_name_can_still_be_redeployed() {
    let h = harness().await;
    h.state
        .cluster
        .submit(cl_raft::Request::DeployStack {
            name: "Old Stack".into(),
            spec: "services:\n  web:\n    image: nginx:1\n".into(),
            at: 0,
        })
        .await
        .expect("seed stack");

    assert_eq!(deploy(&h, "Old%20Stack", PLAIN_SPEC).await, StatusCode::OK);
    let state = h.state.cluster.state().await;
    assert_eq!(state.stacks["Old Stack"].revision, 2);
    assert_eq!(
        deploy(&h, "New%20Stack", PLAIN_SPEC).await,
        StatusCode::BAD_REQUEST
    );
}

/// `<stack>_<network>` is ambiguous when both halves hold an underscore:
/// stack `a_b` on `internal` and stack `a` on `b_internal` would share one
/// overlay (subnet, bridge, docker network), so the second deploy is refused.
#[tokio::test]
async fn deploying_a_stack_whose_overlay_collides_with_another_is_rejected() {
    let h = harness().await;
    {
        let mut cfg = h.state.cluster_config.write().await;
        let net = cfg.networks.get("internal").unwrap().clone();
        cfg.networks.insert("b_internal".to_string(), net);
    }
    let on = |net: &str| {
        let spec = format!("services:\n  web:\n    image: nginx\n    networks: [{net}]\nnetworks:\n  {net}: {{}}\n");
        format!(r#"{{"spec":{spec:?}}}"#)
    };

    assert_eq!(deploy(&h, "a_b", &on("internal")).await, StatusCode::OK);
    assert_eq!(
        deploy(&h, "a", &on("b_internal")).await,
        StatusCode::CONFLICT
    );
    assert!(!h.state.cluster.state().await.stacks.contains_key("a"));

    // Redeploying the stack that already owns the overlay is not a collision.
    assert_eq!(deploy(&h, "a_b", &on("internal")).await, StatusCode::OK);
}

/// Renaming public network `x` to `b_pub` would move stack `a`'s overlay onto
/// `a_b_pub`, the name stack `a_b` (on `pub`) already has: refused, and the
/// config is left alone.
#[tokio::test]
async fn renaming_a_network_into_an_overlay_collision_is_rejected() {
    let h = harness().await;
    {
        let mut cfg = h.state.cluster_config.write().await;
        let net = cfg.networks.get_mut("internal").unwrap();
        net.subnet = Some("10.10.0.0/24".parse().unwrap());
        net.domain = Some("test.local".to_string());
        cfg.nodes.get_mut("n0").unwrap().network.insert(
            "internal".to_string(),
            cl_config::NodeAddress::Ip("10.10.0.1".parse().unwrap()),
        );
        for name in ["pub", "x"] {
            cfg.networks.insert(
                name.to_string(),
                Network {
                    kind: NetworkKind::Public,
                    subnet: None,
                    domain: None,
                    publish_dns: true,
                    access: None,
                    managed: false,
                    open_ports: Vec::new(),
                    proxy: false,
                    container: None,
                },
            );
        }
    }
    for (name, net) in [("a_b", "pub"), ("a", "x")] {
        let spec = format!(
            "services:\n  web:\n    image: nginx\n    networks: [{net}]\nnetworks:\n  {net}: {{}}\n"
        );
        h.state
            .cluster
            .submit(cl_raft::Request::DeployStack {
                name: name.into(),
                spec,
                at: 0,
            })
            .await
            .expect("deploy stack");
    }

    let resp = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/networks/x/rename")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"new_name":"b_pub"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert!(h
        .state
        .cluster_config
        .read()
        .await
        .networks
        .contains_key("x"));
}

/// A redeploy that doesn't resend a file-backed config's content must not
/// forget the stack owns it: `SetStackObjects` used to be handed exactly
/// whatever the request's `configs` map contained, so a client that only
/// resends the spec (every browser redeploy, before this test) silently
/// dropped the stack's claim on configs it never touched.
#[tokio::test]
async fn redeploying_without_resending_a_config_keeps_it() {
    let h = harness().await;
    let spec = "services:\n  web:\n    image: nginx\n    configs: [site_conf]\nconfigs:\n  site_conf:\n    file: ./site.conf\n";

    let status = deploy(
        &h,
        "site",
        &format!(r#"{{"spec":{spec:?},"configs":{{"site_conf":"aGVsbG8="}}}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let state = h.state.cluster.state().await;
    assert!(state.configs.contains_key("site_conf"));
    assert!(state.stacks["site"].configs.contains("site_conf"));
    drop(state);

    // Redeploy the same spec, this time without resending the config.
    let status = deploy(&h, "site", &format!(r#"{{"spec":{spec:?}}}"#)).await;
    assert_eq!(status, StatusCode::OK);

    let state = h.state.cluster.state().await;
    assert!(
        state.configs.contains_key("site_conf"),
        "config must not be dropped"
    );
    assert!(
        state.stacks["site"].configs.contains("site_conf"),
        "stack must still own the config it never stopped referencing"
    );
}

/// A config a service references without bundling a `file:` for it (it must
/// already exist, created separately) is never "owned" by the stack, but the
/// ui still needs to know the stack uses it -- that's what `used_configs`
/// is for, as opposed to the owned-only `configs`.
#[tokio::test]
async fn a_config_referenced_without_a_file_shows_up_in_used_configs_not_owned_configs() {
    let h = harness().await;
    h.state
        .cluster
        .submit(cl_raft::Request::PutConfig {
            name: "shared_conf".to_string(),
            content: cl_raft::base64_encode(b"hello"),
            owner: None,
        })
        .await
        .expect("seed the pre-existing config");

    let spec = "services:\n  web:\n    image: nginx\n    configs: [shared_conf]\nconfigs:\n  shared_conf: {}\n";
    let status = deploy(&h, "site", &format!(r#"{{"spec":{spec:?}}}"#)).await;
    assert_eq!(status, StatusCode::OK);

    let state = h.state.cluster.state().await;
    let stack = &state.stacks["site"];
    assert!(
        !stack.configs.contains("shared_conf"),
        "the stack does not own it"
    );
    assert!(
        stack.used_configs.contains("shared_conf"),
        "but the stack does use it"
    );
}

/// A brand new file-backed config with no content anywhere is a request the
/// server must reject up front, not accept and leave half-wired.
#[tokio::test]
async fn deploying_a_new_file_config_with_no_content_is_rejected() {
    let h = harness().await;
    let spec = "services:\n  web:\n    image: nginx\n    configs: [site_conf]\nconfigs:\n  site_conf:\n    file: ./site.conf\n";

    let status = deploy(&h, "site", &format!(r#"{{"spec":{spec:?}}}"#)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let state = h.state.cluster.state().await;
    assert!(!state.stacks.contains_key("site"));
}
