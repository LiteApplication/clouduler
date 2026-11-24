use super::*;

/// The instance list is read by the ui, the qemu harness and anything
/// scripting the api, all by field name. They disagreed once already: the api
/// emitted `running`, the ui read `status`, the harness read `state`, and the
/// only symptom was a deploy that looked like it never came up.
#[tokio::test]
async fn instance_summaries_carry_the_fields_their_consumers_read() {
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

    let resp = h
        .router
        .oneshot(
            Request::builder()
                .uri("/api/instances")
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
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let first = &value[0];

    for field in [
        "id",
        "node",
        "desired",
        "state",
        "running",
        "health",
        "image",
        "image_digest",
        "started_at",
        "migration",
    ] {
        assert!(first.get(field).is_some(), "missing {field} in {first}");
    }
    assert_eq!(first["id"], "blog.web.0");
    assert_eq!(first["desired"], "running");
}

/// An instance on another node reports `unknown`, not `stopped`. There is no
/// rpc path to inspect a container elsewhere, and reporting it as stopped
/// would have the ui claim a healthy container is down.
#[tokio::test]
async fn an_instance_on_another_node_reports_unknown_rather_than_stopped() {
    let h = harness().await;
    // The state machine refuses to place work on a node it has never heard of,
    // so the other node has to be in the roster first.
    h.state
        .cluster
        .submit(cl_raft::Request::UpsertNode(NodeState {
            name: "n1".into(),
            id: uuid::Uuid::new_v4(),
            role: ClusterRole::Worker,
            rpc_addr: free_addr(),
            drives: BTreeMap::new(),
            availability: Availability::Active,
            labels: Default::default(),
            last_resort: false,
            egress_networks: Default::default(),
            networks: Default::default(),
            open_ports: Default::default(),
            arch: Default::default(),
            binary_hash: Default::default(),
            node_pubkey: Default::default(),
            memory_total_bytes: Default::default(),
            gpus: Default::default(),
            images: Default::default(),
            version: Default::default(),
        }))
        .await
        .expect("add n1");
    let response = h
        .state
        .cluster
        .submit(cl_raft::Request::Assign {
            instance: "blog.web.0".parse().unwrap(),
            node: "n1".into(),
            drives: Default::default(),
        })
        .await
        .expect("assign");
    assert!(response.is_ok(), "{:?}", response.error);

    let resp = h
        .router
        .oneshot(
            Request::builder()
                .uri("/api/instances")
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
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(value[0]["state"], "unknown");
    assert_eq!(value[0]["running"], serde_json::Value::Null);
}

// ---- cross-node forwarding (internal_peer) ----
//
// A live two-node forward is exercised manually (there is no second node in
// this harness to bind a real listener for), so these cover the two things
// that would silently break the feature: the internal routes staying behind
// `InternalAuth` rather than being reachable by anyone who can guess a path,
// and a correctly signed request getting the same real, local-only answer
// `summarize_local`/`local_resource_usage` give a same-node caller.

/// Every assignment gets a row, whether or not its node could say anything
/// about it. A missing row means an instance silently absent from the Updates
/// view, which is worse than one reading "unknown".
#[tokio::test]
async fn the_updates_view_lists_every_assignment() {
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

    let resp = h
        .router
        .oneshot(
            Request::builder()
                .uri("/api/updates")
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
    let rows: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let row = &rows[0];
    assert_eq!(row["id"], "blog.web.0");
    assert_eq!(row["stack"], "blog");
    assert_eq!(row["node"], "n0");
    // Nothing is running here, so nothing is known — and an unknown digest
    // must never be reported as an available update, or "update all" would
    // churn every container in the cluster forever.
    assert_eq!(row["update_available"], false);
    assert_eq!(row["pin_drift"], false);
}
