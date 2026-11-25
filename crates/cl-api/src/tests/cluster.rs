use super::*;

#[tokio::test]
async fn cluster_status_has_the_documented_shape() {
    let h = harness().await;
    let resp = h
        .router
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
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    // The ui reads every one of these by name (ui/src/api/types.ts,
    // views/Overview.tsx), so a rename here breaks the overview panel silently.
    for field in [
        "node",
        "is_leader",
        "has_quorum",
        "leader",
        "voters",
        "reachable",
        "managers_total",
        "managers_reachable",
        "version",
        "state_version",
    ] {
        assert!(value.get(field).is_some(), "missing {field} in {value}");
    }

    // A node name, not the raft id, which is an internal hash nobody can act on.
    assert_eq!(value["leader"], "n0");
    assert_eq!(value["node"], "n0");
    assert_eq!(value["managers_total"], 1);
    assert_eq!(value["managers_reachable"], 1);
}

/// A node whose dockerd is down or slow to start still has to serve the api:
/// the web ui is where someone would go to find out why, and a manager
/// without docker is a supported setup in its own right. The daemon used to
/// skip starting the api entirely in that case, which took the whole node off
/// the air after a reboot that raced dockerd.
#[tokio::test]
async fn the_api_answers_without_a_docker_daemon() {
    let h = harness().await;
    h.state.docker.set(None).await;

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

    // Container detail is the part that genuinely cannot be answered, and it
    // says so with a retryable status rather than a 500.
    let resp = h
        .router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/instances/blog.web.0/restart")
                .header("authorization", format!("Bearer {}", h.admin_key))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::SERVICE_UNAVAILABLE || resp.status() == StatusCode::NOT_FOUND,
        "unexpected status {}",
        resp.status()
    );
}

#[tokio::test]
async fn nodes_list_reflects_the_cluster_state() {
    let h = harness().await;
    let resp = h
        .router
        .oneshot(
            Request::builder()
                .uri("/api/nodes")
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
    let nodes = value.as_array().unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["name"], "n0");
}

fn loopback_node(rpc_port: u16) -> cl_config::Node {
    cl_config::Node {
        id: uuid::Uuid::new_v4(),
        role: cl_config::NodeRole::Worker,
        network: BTreeMap::from([(
            "internal".to_string(),
            cl_config::NodeAddress::Ip("127.0.0.1".parse().unwrap()),
        )]),
        drives: BTreeMap::new(),
        labels: BTreeMap::new(),
        rpc_port,
        data_dir: None,
        docker_data_dir: None,
        generate_secret_key: None,
        last_resort: false,
        disk_probe: true,
    }
}

/// A peer whose raft port accepts connections is a healthy link; one whose
/// port is closed is a broken one, and the two must not be confused.
#[tokio::test]
async fn connectivity_marks_a_closed_peer_broken_and_an_open_one_healthy() {
    let h = harness().await;
    // A real raft peer: it proves the cluster key, then answers `Ping`. A
    // listener that merely accepted sockets would (correctly) read as down.
    let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let open_port = open.local_addr().unwrap().port();
    let key = h.state.key.read().await.clone().unwrap();
    tokio::spawn(async move {
        use futures::{SinkExt, StreamExt};
        loop {
            let Ok((mut stream, _)) = open.accept().await else {
                return;
            };
            let key = key.clone();
            tokio::spawn(async move {
                if cl_secret::stream_auth::accept(&mut stream, &key, cl_raft::network::AUTH_LABEL)
                    .await
                    .is_err()
                {
                    return;
                }
                let mut conn = cl_raft::rpc::frame(stream);
                while conn.next().await.is_some() {
                    let pong = cl_raft::rpc::encode(&cl_raft::rpc::RpcResponse::Pong).unwrap();
                    if conn.send(pong).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    let closed_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    {
        let mut cfg = h.state.cluster_config.write().await;
        cfg.nodes.insert("up".into(), loopback_node(open_port));
        cfg.nodes.insert("down".into(), loopback_node(closed_port));
    }

    for (i, up) in [true, false, true, false, true].into_iter().enumerate() {
        h.state.link_health.record(
            "down",
            cl_sched::LinkSample {
                at: 1000 + i as i64,
                raft: up,
                api: up,
                drive: up,
                raft_ms: None,
            },
        );
    }

    let resp = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/nodes/connectivity")
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
    let reports = value["reports"].as_array().unwrap();
    assert_eq!(reports.len(), 3);

    let n0 = reports.iter().find(|r| r["node"] == "n0").unwrap();
    assert_eq!(n0["reported"], true);
    let links = n0["links"].as_array().unwrap();
    assert_eq!(links.len(), 2, "n0 sees every peer but itself");
    let up = links.iter().find(|l| l["peer"] == "up").unwrap();
    let down = links.iter().find(|l| l["peer"] == "down").unwrap();
    assert_eq!(up["raft"], true);
    assert!(up["raft_ms"].is_u64());
    assert_eq!(down["raft"], false);
    assert!(down["raft_ms"].is_null());
    // Neither peer has drives, so the drive-sync port is not probed and counts as up.
    assert_eq!(up["drive"], true);
    assert_eq!(down["drive"], true);
    // The history comes from the recorded rounds, not from this request's probe.
    assert_eq!(down["state"], "flapping");
    assert_eq!(down["failures"], 2);
    assert_eq!(down["history"].as_array().unwrap().len(), 5);
    assert_eq!(down["history"][1]["at"], 1001);
    assert_eq!(up["state"], "ok");
    assert!(up["history"].as_array().unwrap().is_empty());

    // Neither peer runs an api here, so a node that cannot be asked is
    // reported as such rather than silently dropped or shown as healthy.
    for name in ["up", "down"] {
        let r = reports.iter().find(|r| r["node"] == name).unwrap();
        assert_eq!(r["reported"], false, "{name} has no report");
        assert_eq!(r["links"].as_array().unwrap().len(), 0);
    }
}

#[tokio::test]
async fn the_internal_links_route_needs_a_signature_and_answers_the_local_view() {
    let h = harness().await;
    let path = "/api/nodes/internal/links";
    let resp = h
        .router
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = signed_get(&h, path).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["node"], "n0");
    assert_eq!(value["links"].as_array().unwrap().len(), 0);
}

/// Mirrors what a join does after a node was deleted and rejoins under the
/// same name: `clouduler init` mints a fresh id for the new instance, so the
/// old instance's tombstone (keyed by id, not name) never blocks it, and
/// `seed_roster` admits it like any other new node. The old id stays
/// tombstoned forever.
#[tokio::test]
async fn rejoining_under_a_removed_name_is_not_blocked_by_the_old_tombstone() {
    let h = harness().await;
    let old_id = h.n0_id;

    h.state
        .cluster
        .submit(cl_raft::Request::RemoveNode { node: "n0".into() })
        .await
        .expect("submit");
    assert!(h.state.cluster.state().await.removed.contains(&old_id));

    // Simulate the config as it looks right after the node was deleted
    // (stripped of the node, same as what `remove()` would have applied),
    // then re-add "n0" the way a rejoin does: same name, brand new id.
    let mut full_config = h.state.cluster_config.read().await.clone();
    {
        let net = full_config.networks.get_mut("internal").unwrap();
        net.subnet = Some("10.10.0.0/24".parse().unwrap());
        net.domain = Some("test.local".to_string());
    }
    let new_id = uuid::Uuid::new_v4();
    assert_ne!(new_id, old_id);
    {
        let n0 = full_config.nodes.get_mut("n0").unwrap();
        n0.id = new_id;
        n0.network.insert(
            "internal".to_string(),
            cl_config::NodeAddress::Ip("10.10.0.1".parse().unwrap()),
        );
    }
    let mut stripped = full_config.clone();
    stripped.nodes.clear();
    *h.state.cluster_config.write().await = stripped;

    let body = serde_json::to_string(&serde_json::json!({"config": full_config})).unwrap();
    let resp = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config")
                .header("authorization", format!("Bearer {}", h.admin_key))
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
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );

    // The old instance's id is still tombstoned - only the new one is admitted.
    assert!(h.state.cluster.state().await.removed.contains(&old_id));
    assert!(!h.state.cluster.state().await.removed.contains(&new_id));
}

/// A brand new cluster mints its first api key once it has a leader.
///
/// The api starts before any election finishes, so the write has nowhere to go
/// on the first attempt. Getting this wrong means a fresh cluster has no way in
/// at all, which is exactly what happened before it was made a retrying task.
#[tokio::test]
async fn a_fresh_cluster_mints_a_bootstrap_key_once_a_leader_exists() {
    let h = harness().await;

    // Start from a cluster holding no keys at all, as a fresh one would.
    h.state
        .cluster
        .submit(cl_raft::Request::RemoveConfig {
            name: keys::API_KEYS_CONFIG.to_string(),
        })
        .await
        .expect("clear keys");
    let empty = keys::ApiKeyStore::load(&h.state.cluster.state().await);
    assert!(empty.keys.is_empty(), "precondition: no keys");

    crate::routes::auth_keys::spawn_bootstrap_first_key(h.state.clone());

    let mut minted = Vec::new();
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        minted = keys::ApiKeyStore::load(&h.state.cluster.state().await).keys;
        if !minted.is_empty() {
            break;
        }
    }

    assert_eq!(minted.len(), 1, "exactly one bootstrap key");
    assert_eq!(minted[0].name.as_deref(), Some("bootstrap"));
}

/// Two managers coming up together must not each mint their own, or the log
/// shows two keys and only one of them is the one that got stored.
#[tokio::test]
async fn the_bootstrap_key_is_only_minted_once() {
    let h = harness().await;
    h.state
        .cluster
        .submit(cl_raft::Request::RemoveConfig {
            name: keys::API_KEYS_CONFIG.to_string(),
        })
        .await
        .expect("clear keys");

    for _ in 0..3 {
        crate::routes::auth_keys::spawn_bootstrap_first_key(h.state.clone());
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    let keys = keys::ApiKeyStore::load(&h.state.cluster.state().await).keys;
    assert_eq!(keys.len(), 1, "expected one key, got {}", keys.len());
}
