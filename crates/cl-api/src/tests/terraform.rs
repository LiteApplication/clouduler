use super::*;

async fn tf_request(h: &Harness, method: &str, uri: &str, body: &str) -> (StatusCode, String) {
    let resp = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn terraform_modules_deploy_list_run_and_delete() {
    let h = harness().await;
    let body = serde_json::json!({
        "spec": "services:\n  web:\n    image: nginx\n",
        "terraform": {"web": "variable \"x\" {}\n"},
    })
    .to_string();
    assert_eq!(deploy(&h, "site", &body).await, StatusCode::OK);

    let (status, json) = tf_request(&h, "GET", "/api/stacks/site/terraform", "").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["enabled_cluster"], false);
    assert_eq!(v["services"][0]["service"], "web");
    assert_eq!(v["services"][0]["revision"], 1);
    assert_eq!(v["services"][0]["enabled"], true);
    assert!(v["services"][0]["last_run"].is_null());

    // Runs are refused while the cluster has no terraform section.
    let (status, _) = tf_request(&h, "POST", "/api/stacks/site/terraform/web/plan", "").await;
    assert_eq!(status, StatusCode::CONFLICT);

    h.state.cluster_config.write().await.terraform = Some(Default::default());
    let (status, _) = tf_request(&h, "POST", "/api/stacks/site/terraform/web/apply", "").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _) = tf_request(&h, "POST", "/api/stacks/site/terraform/nope/apply", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, json) = tf_request(&h, "GET", "/api/stacks/site/terraform", "").await;
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["enabled_cluster"], true);
    assert_eq!(v["services"][0]["pending_request"], "apply");

    // A redeploy without `terraform` keeps the module; an edit bumps the revision.
    assert_eq!(deploy(&h, "site", PLAIN_SPEC).await, StatusCode::OK);
    let (status, _) = tf_request(
        &h,
        "PUT",
        "/api/stacks/site/terraform/web",
        r#"{"source":"variable \"y\" {}\n"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let state = h.state.cluster.state().await;
    assert_eq!(state.terraform_modules["site"]["web"].revision, 2);
    drop(state);

    let (status, _) = tf_request(&h, "DELETE", "/api/stacks/site/terraform/web", "").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(h.state.cluster.state().await.terraform_modules.is_empty());
    let (status, _) = tf_request(&h, "DELETE", "/api/stacks/site/terraform/web", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn terraform_modules_with_a_backend_or_bad_hcl_are_rejected() {
    let h = harness().await;
    assert_eq!(deploy(&h, "site", PLAIN_SPEC).await, StatusCode::OK);
    for source in [
        "terraform {\n  backend \"s3\" {}\n}\n",
        "this is = = not hcl {",
    ] {
        let body = serde_json::json!({ "source": source }).to_string();
        let (status, _) = tf_request(&h, "PUT", "/api/stacks/site/terraform/web", &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{source}");
        let body = serde_json::json!({
            "spec": "services:\n  web:\n    image: nginx\n",
            "terraform": {"web": source},
        })
        .to_string();
        assert_eq!(deploy(&h, "site", &body).await, StatusCode::BAD_REQUEST);
    }
    assert!(h.state.cluster.state().await.terraform_modules.is_empty());
}

#[tokio::test]
async fn terraform_outputs_are_listed_and_reserved_targets_refused() {
    use cl_raft::terraform::{
        TerraformConfigOutput, TerraformMode, TerraformOutcome, TerraformRun, TerraformSecretOutput,
    };
    let h = harness().await;
    let spec = "services:\n  web:\n    image: nginx\n    x-clouduler:\n      terraform:\n        outputs:\n          - {output: id, config: oidc-id}\n          - {output: sec, secret: oidc-sec}\n";
    let body =
        serde_json::json!({"spec": spec, "terraform": {"web": "variable \"x\" {}\n"}}).to_string();
    assert_eq!(deploy(&h, "site", &body).await, StatusCode::OK);

    let run = TerraformRun {
        id: "r1".into(),
        mode: TerraformMode::Apply,
        trigger: "manual".into(),
        node: "n1".into(),
        instance: cl_raft::ids::InstanceId::new("site", "web", 0),
        fingerprint: "f".into(),
        started_at: 1,
        finished_at: None,
        outcome: TerraformOutcome::Running,
        log_tail: String::new(),
    };
    h.state
        .cluster
        .submit(cl_raft::Request::RecordTerraformRun { run })
        .await
        .unwrap();
    let r = h
        .state
        .cluster
        .submit(cl_raft::Request::PutTerraformOutputs {
            key: "site/web".into(),
            run_id: "r1".into(),
            secrets: vec![TerraformSecretOutput {
                name: "oidc-sec".into(),
                sealed: "c2VhbGVk".into(),
                key_fingerprint: "fp".into(),
            }],
            configs: vec![TerraformConfigOutput {
                name: "oidc-id".into(),
                content: "aWQ=".into(),
            }],
            claims: vec![],
        })
        .await
        .unwrap();
    assert!(r.error.is_none(), "{:?}", r.error);
    let (_, json) = tf_request(&h, "GET", "/api/stacks/site/terraform", "").await;
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let outputs = v["services"][0]["outputs"].as_array().unwrap();
    assert_eq!(outputs.len(), 2);
    assert!(outputs
        .iter()
        .any(|o| o["kind"] == "secret" && o["name"] == "oidc-sec" && o["revision"] == 1));
    assert!(outputs
        .iter()
        .any(|o| o["kind"] == "config" && o["name"] == "oidc-id"));

    let reserved = format!(
        "services:\n  web:\n    image: nginx\n    x-clouduler:\n      terraform:\n        outputs:\n          - {{output: id, config: {}}}\n",
        cl_update::plan::RUN_CONFIG
    );
    let body = serde_json::json!({ "spec": reserved }).to_string();
    assert_eq!(deploy(&h, "other", &body).await, StatusCode::BAD_REQUEST);
}
