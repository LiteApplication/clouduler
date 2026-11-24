use super::*;

#[tokio::test]
async fn redirects_can_be_created_listed_and_removed() {
    let h = harness().await;
    let body = r#"{"host":"old","network":"internal","target":"https://new.example.com","preserve_path":true,"status":301}"#;

    let resp = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/redirects")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["target"], "https://new.example.com");

    let resp = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/redirects")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    let listed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let resp = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/redirects/{id}"))
                .header("authorization", format!("Bearer {}", h.admin_key))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn renaming_a_network_updates_config_stacks_and_redirects() {
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
    }

    let redirect_body =
        r#"{"host":"old","network":"internal","target":"https://new.example.com","status":301}"#;
    let resp = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/redirects")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(redirect_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let spec = "services:\n  web:\n    image: nginx\n    networks: [internal]\nnetworks:\n  internal: {}\n";
    h.state
        .cluster
        .submit(cl_raft::Request::DeployStack {
            name: "site".into(),
            spec: spec.into(),
            at: 0,
        })
        .await
        .expect("deploy stack");

    let resp = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/networks/internal/rename")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"new_name":"core"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let cfg = h.state.cluster_config.read().await;
    assert!(cfg.networks.contains_key("core"));
    assert!(!cfg.networks.contains_key("internal"));
    assert_eq!(cfg.web.network, "core");
    drop(cfg);

    let state = h.state.cluster.state().await;
    let stack = state.stacks.get("site").expect("stack still exists");
    assert!(stack.spec.contains("core"));
    assert!(!stack.spec.contains("internal"));

    let store = crate::redirects::RedirectStore::load(&state);
    assert_eq!(store.rules[0].network, "core");
}

#[tokio::test]
async fn renaming_a_drive_updates_config_and_stacks() {
    let h = harness().await;

    {
        let mut cfg = h.state.cluster_config.write().await;
        cfg.drives.insert(
            "fast".to_string(),
            cl_config::Drive {
                sync_interval: Duration::from_secs(900),
                mode: cl_config::DriveMode::Replicated,
                served_by: vec![],
                failback: Default::default(),
                max_parallel_syncs: 1,
            },
        );
        cfg.nodes.insert(
            "n0".into(),
            cl_config::Node {
                id: uuid::Uuid::new_v4(),
                role: cl_config::NodeRole::Manager,
                network: BTreeMap::from([(
                    "internal".to_string(),
                    cl_config::NodeAddress::Ip("10.10.0.1".parse().unwrap()),
                )]),
                drives: BTreeMap::from([("fast".to_string(), "/mnt/fast".into())]),
                labels: BTreeMap::new(),
                rpc_port: cl_config::DEFAULT_RPC_PORT,
                data_dir: None,
                docker_data_dir: None,
                generate_secret_key: None,
                last_resort: false,
                disk_probe: true,
            },
        );
        let net = cfg.networks.get_mut("internal").unwrap();
        net.subnet = Some("10.10.0.0/24".parse().unwrap());
        net.domain = Some("test.local".to_string());
    }

    let spec = "services:\n  web:\n    image: nginx\n    volumes: [fast/data:/data]\n";
    h.state
        .cluster
        .submit(cl_raft::Request::DeployStack {
            name: "site".into(),
            spec: spec.into(),
            at: 0,
        })
        .await
        .expect("deploy stack");

    let resp = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/drives/fast/rename")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"new_name":"bulk"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let cfg = h.state.cluster_config.read().await;
    assert!(cfg.drives.contains_key("bulk"));
    assert!(!cfg.drives.contains_key("fast"));
    assert_eq!(
        cfg.nodes["n0"]
            .drives
            .get("bulk")
            .map(|p| p.to_str().unwrap()),
        Some("/mnt/fast")
    );
    drop(cfg);

    let state = h.state.cluster.state().await;
    let stack = state.stacks.get("site").expect("stack still exists");
    assert!(stack.spec.contains("bulk/data"));
    assert!(!stack.spec.contains("fast/data"));
}

#[tokio::test]
async fn a_redirect_with_an_unknown_network_is_rejected() {
    let h = harness().await;
    let body = r#"{"host":"old","network":"nope","target":"https://new.example.com","status":301}"#;

    let resp = h
        .router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/redirects")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_redirect_with_an_invalid_status_is_rejected() {
    let h = harness().await;
    let body =
        r#"{"host":"old","network":"internal","target":"https://new.example.com","status":200}"#;

    let resp = h
        .router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/redirects")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
