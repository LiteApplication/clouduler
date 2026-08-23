//! Route level tests: a real single node raft cluster, a lazily connected
//! docker client (never actually dialed, since none of the routes exercised
//! here touch it), driven through the router with `tower::ServiceExt::oneshot`.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use cl_config::{ClusterConfig, ClusterMeta, Network, NetworkKind, WebConfig};
use cl_docker::Docker;
use cl_raft::node::{ClusterNode, Options};
use cl_raft::state::{Availability, NodeState, Role as ClusterRole};
use cl_secret::ClusterKey;
use openraft::BasicNode;
use tempfile::TempDir;
use tokio::sync::{Mutex, RwLock};
use tower::ServiceExt;

use crate::auth::session::SessionStore;
use crate::auth::{keys, Identity, Role};
use crate::server::{build_router, AppState};

fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr")
}

/// Everything a test needs: the router, plus handles to poke at auth and
/// session state directly rather than through http.
struct Harness {
    router: Router,
    state: AppState,
    _dir: TempDir,
    admin_key: String,
    n0_id: uuid::Uuid,
}

async fn harness() -> Harness {
    let dir = TempDir::new().expect("tempdir");
    let addr = free_addr();
    let n0_id = uuid::Uuid::new_v4();
    let node = ClusterNode::start(Options {
        name: "n0".into(),
        id: n0_id,
        rpc_addr: addr,
        data_dir: dir.path().join("n0"),
        cluster_name: "test".into(),
    })
    .await
    .expect("start node");

    let members = BTreeMap::from([(cl_raft::node_id(n0_id), BasicNode::new(addr.to_string()))]);
    node.initialize(members).await.expect("initialize");
    for _ in 0..200 {
        if node.is_leader() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(node.is_leader(), "single node cluster must elect itself leader");

    node.submit(cl_raft::Request::UpsertNode(NodeState {
        name: "n0".into(),
        id: n0_id,
        role: ClusterRole::Manager,
        rpc_addr: addr,
        drives: BTreeMap::new(),
        availability: Availability::Active,
        labels: Default::default(),
        egress_networks: Default::default(),
        networks: Default::default(),
        open_ports: Default::default(),
        arch: Default::default(),
        binary_hash: Default::default(),
        node_pubkey: Default::default(),
    }))
    .await
    .expect("submit");

    let new_key = keys::generate(Some("test".into()), 0);
    let store = keys::ApiKeyStore {
        keys: vec![new_key.record],
    };
    node.submit(cl_raft::Request::PutConfig {
        name: keys::API_KEYS_CONFIG.to_string(),
        content: store.encode(),
        owner: None,
    })
    .await
    .expect("submit keys");

    let cluster_config = ClusterConfig {
        cluster: ClusterMeta { name: "test".into() },
        networks: BTreeMap::from([(
            "internal".to_string(),
            Network {
                kind: NetworkKind::Internal,
                subnet: None,
                domain: None,
            },
        )]),
        drives: BTreeMap::new(),
        nodes: BTreeMap::from([(
            "n0".to_string(),
            cl_config::Node {
                id: n0_id,
                role: cl_config::NodeRole::Manager,
                network: BTreeMap::from([("internal".to_string(), cl_config::NodeAddress::Ip("127.0.0.1".parse().unwrap()))]),
                drives: BTreeMap::new(),
                labels: BTreeMap::new(),
                rpc_port: cl_config::DEFAULT_RPC_PORT,
                data_dir: None,
                generate_secret_key: None,
            },
        )]),
        web: WebConfig {
            address: "cloud".into(),
            network: "internal".into(),
            port: 8443,
            through_proxy: true,
            backend_port: 8444,
        },
        data_dir: dir.path().to_path_buf(),
        failover: Default::default(),
        secrets: Default::default(),
        auth: Default::default(),
        cloudflare: None,
        proxy: Default::default(),
        self_update: None,
    };

    let (shutdown, _shutdown_rx) = tokio::sync::watch::channel(());
    let state = AppState {
        cluster: Arc::new(node),
        docker: Arc::new(RwLock::new(Docker::connect().ok())),
        node_name: "n0".to_string(),
        config_path: dir.path().join("cluster.yaml"),
        cluster_config: Arc::new(RwLock::new(cluster_config)),
        key_path: dir.path().join("secrets.key"),
        key: Arc::new(RwLock::new(Some(ClusterKey::generate()))),
        sessions: Arc::new(SessionStore::new()),
        oidc: None,
        pending_edit: Arc::new(Mutex::new(None)),
        started_at: Instant::now(),
        session_ttl: Duration::from_secs(3600),
        rollback_after: crate::config_edit::DEFAULT_ROLLBACK_AFTER,
        self_update_artifact_path: dir.path().join("self-update-artifact"),
        shutdown,
        http: reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .expect("building the test http client"),
        tls_fingerprint: "test:fingerprint".to_string(),
    };

    let router = build_router(state.clone());
    Harness {
        router,
        state,
        _dir: dir,
        admin_key: new_key.secret,
        n0_id,
    }
}

async fn viewer_cookie(state: &AppState) -> String {
    let id = state
        .sessions
        .create(
            Identity {
                subject: "viewer@example.com".into(),
                role: Role::Viewer,
            },
            Duration::from_secs(60),
        )
        .await;
    format!("cl_session={id}")
}

#[tokio::test]
async fn unauthenticated_requests_are_rejected() {
    let h = harness().await;
    let resp = h
        .router
        .oneshot(Request::builder().uri("/api/status").body(Body::empty()).unwrap())
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

    assert_eq!(put("cf-token-1").await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(put("cf-token-2").await.unwrap().status(), StatusCode::NO_CONTENT);

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

    assert_eq!(put("oidc-secret-1").await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(put("oidc-secret-2").await.unwrap().status(), StatusCode::NO_CONTENT);

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
        &http_body_util::BodyExt::collect(status.into_body()).await.unwrap().to_bytes(),
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
    assert!(!text.contains("hunter2"), "a secret value must never be returned");
}

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
    *h.state.docker.write().await = None;

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
    let bytes = http_body_util::BodyExt::collect(resp.into_body()).await.unwrap().to_bytes();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&bytes));

    // The old instance's id is still tombstoned - only the new one is admitted.
    assert!(h.state.cluster.state().await.removed.contains(&old_id));
    assert!(!h.state.cluster.state().await.removed.contains(&new_id));
}

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
    let bytes = http_body_util::BodyExt::collect(resp.into_body()).await.unwrap().to_bytes();
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
    let bytes = http_body_util::BodyExt::collect(resp.into_body()).await.unwrap().to_bytes();
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

    let redirect_body = r#"{"host":"old","network":"internal","target":"https://new.example.com","status":301}"#;
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
        .submit(cl_raft::Request::DeployStack { name: "site".into(), spec: spec.into() })
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
            },
        );
        cfg.nodes.insert(
            "n0".into(),
            cl_config::Node {
                id: uuid::Uuid::new_v4(),
                role: cl_config::NodeRole::Manager,
                network: BTreeMap::from([("internal".to_string(), cl_config::NodeAddress::Ip("10.10.0.1".parse().unwrap()))]),
                drives: BTreeMap::from([("fast".to_string(), "/mnt/fast".into())]),
                labels: BTreeMap::new(),
                rpc_port: cl_config::DEFAULT_RPC_PORT,
                data_dir: None,
                generate_secret_key: None,
            },
        );
        let net = cfg.networks.get_mut("internal").unwrap();
        net.subnet = Some("10.10.0.0/24".parse().unwrap());
        net.domain = Some("test.local".to_string());
    }

    let spec = "services:\n  web:\n    image: nginx\n    volumes: [fast/data:/data]\n";
    h.state
        .cluster
        .submit(cl_raft::Request::DeployStack { name: "site".into(), spec: spec.into() })
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
    assert_eq!(cfg.nodes["n0"].drives.get("bulk").map(|p| p.to_str().unwrap()), Some("/mnt/fast"));
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
    let body = r#"{"host":"old","network":"internal","target":"https://new.example.com","status":200}"#;

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
        "id", "node", "desired", "state", "running", "health", "image", "image_digest",
        "started_at", "migration",
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
            egress_networks: Default::default(),
            networks: Default::default(),
            open_ports: Default::default(),
            arch: Default::default(),
            binary_hash: Default::default(),
            node_pubkey: Default::default(),
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

    for path in ["/api/instances/internal/blog.web.0/summary", "/api/nodes/internal/n0/usage"] {
        let resp = h
            .router
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{path} answered without a signature");
    }
}

async fn signed_get(h: &Harness, path: &str) -> axum::response::Response {
    let key_guard = h.state.key.read().await;
    let signature = cl_secret::internal_auth::sign(key_guard.as_ref().expect("test cluster holds a key"), path);
    drop(key_guard);
    h.router
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header(cl_update::internal_client::AUTH_HEADER, signature)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
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

// ---- the api/ui contract ----
//
// Three separate bugs shipped because a field was renamed on one side of the
// wire and not the other: the config rollback countdown (`remaining_seconds`
// vs `seconds_left`), and the overview's quorum banner (`has_quorum` vs
// `quorum`), which showed "Quorum lost" on a healthy cluster for as long as it
// existed. Nothing failed in either language: Rust did not know what the ui
// read, and TypeScript types are erased at runtime, so the ui happily read
// `undefined` and carried on.
//
// This test records the real serialized body of every endpoint the ui reads
// into a fixture the ui's own test suite imports. On the TypeScript side those
// fixtures are *assigned to the interfaces*, so `tsc` fails the build if an
// interface declares a field the server does not actually send. The fixture is
// generated from real responses, so it cannot drift from the server; the
// assignment is checked by the compiler, so it cannot drift from the ui.
//
// Regenerate after an intentional api change:
//
//     UPDATE_API_FIXTURES=1 cargo test -p cl-api api_shapes
const API_SHAPES_FIXTURE: &str = "../../ui/src/api/__fixtures__/api-shapes.json";

/// Pin the values that differ run to run, so the fixture records the shape and
/// not one run's accidents.
///
/// Replacements keep the original JSON type. That matters: the ui assigns these
/// fixtures to its interfaces, and a field typed as a string union (`role`,
/// `type`, `availability`) would stop typechecking if its value were blanked.
/// So this is a short explicit list of volatile keys rather than a blanket
/// "erase every leaf".
fn normalize(value: &mut serde_json::Value, tempdir: &str) {
    /// Random per run: generated ids, hashes, key fingerprints, ephemeral ports.
    const VOLATILE_STRINGS: &[&str] = &["fingerprint", "subject", "rpc_addr", "content", "id"];
    const VOLATILE_NUMBERS: &[&str] = &["state_version"];

    match value {
        serde_json::Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if VOLATILE_STRINGS.contains(&key.as_str()) && v.is_string() {
                    *v = serde_json::json!("<volatile>");
                } else if VOLATILE_NUMBERS.contains(&key.as_str()) && v.is_number() {
                    *v = serde_json::json!(0);
                } else {
                    normalize(v, tempdir);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                normalize(v, tempdir);
            }
        }
        serde_json::Value::String(text) if text.contains(tempdir) => {
            *text = text.replace(tempdir, "<tmp>");
        }
        _ => {}
    }
}

#[tokio::test]
async fn api_shapes_match_the_fixture_the_ui_typechecks_against() {
    let h = harness().await;

    // Give the config a served drive n0 implements, so `/api/drives` returns
    // a real item instead of `[]` -- an empty array can't prove the item
    // shape, which is why it's still skipped for stacks/instances/certificates
    // below. Served (rather than replicated) exercises `active_server` as a
    // string, not just its `null` case.
    {
        let mut cfg = h.state.cluster_config.write().await;
        cfg.drives.insert(
            "fast".to_string(),
            cl_config::Drive {
                sync_interval: Duration::from_secs(900),
                mode: cl_config::DriveMode::Served,
                served_by: vec!["n0".to_string()],
            },
        );
    }
    let node = h.state.cluster.state().await.nodes["n0"].clone();
    h.state
        .cluster
        .submit(cl_raft::Request::UpsertNode(NodeState {
            drives: BTreeMap::from([("fast".to_string(), "/mnt/fast".into())]),
            node_pubkey: Some("target".to_string()),
            ..node
        }))
        .await
        .expect("submit node drives");

    // Same reasoning as the drive above, for every other collection `state`
    // carries: an empty map or array can't prove the item shape underneath it
    // still matches what the ui declares, which is exactly how `labels`
    // drifted onto `NodeState` unnoticed. One real item each, so a rename in
    // any of these gets caught the same way.
    h.state
        .cluster
        .submit(cl_raft::Request::DeployStack {
            name: "web".to_string(),
            spec: "services: {}".to_string(),
        })
        .await
        .expect("submit stack");
    h.state
        .cluster
        .submit(cl_raft::Request::SetStackObjects {
            name: "web".to_string(),
            configs: BTreeSet::from(["web.conf".to_string()]),
            secrets: BTreeSet::from(["web.secret".to_string()]),
        })
        .await
        .expect("submit stack objects");

    h.state
        .cluster
        .submit(cl_raft::Request::Assign {
            instance: cl_raft::InstanceId::new("web", "app", 0),
            node: "n0".to_string(),
            drives: BTreeSet::new(),
        })
        .await
        .expect("submit assign");

    let volume = cl_raft::VolumeId::new("fast", "v0");
    h.state
        .cluster
        .submit(cl_raft::Request::UpsertVolume {
            id: volume.clone(),
            primary: "n0".to_string(),
        })
        .await
        .expect("submit volume");
    h.state
        .cluster
        .submit(cl_raft::Request::SetVolumeReplica {
            id: volume,
            node: "n0".to_string(),
            generation: 1,
        })
        .await
        .expect("submit volume replica");

    h.state
        .cluster
        .submit(cl_raft::Request::PutSecret {
            name: "web.secret".to_string(),
            sealed: "c2VhbGVk".to_string(),
            key_fingerprint: "fp".to_string(),
            owner: Some("web".to_string()),
        })
        .await
        .expect("submit secret");

    h.state
        .cluster
        .submit(cl_raft::Request::StoreCertificate(cl_raft::CertificateState {
            fqdn: "example.test".to_string(),
            chain_pem: "chain".to_string(),
            key_pem: "key".to_string(),
            not_after: 0,
        }))
        .await
        .expect("submit certificate");

    h.state
        .cluster
        .submit(cl_raft::Request::UpsertDnsRecord(cl_raft::DnsRecordState {
            fqdn: "example.test".to_string(),
            record_type: "A".to_string(),
            content: "127.0.0.1".to_string(),
            zone: "test".to_string(),
            provider_id: None,
        }))
        .await
        .expect("submit dns record");

    h.state
        .cluster
        .submit(cl_raft::Request::PutWrappedKey {
            node: "n0".to_string(),
            entry: cl_raft::state::WrappedKeyEntry {
                ephemeral_public: "eph".to_string(),
                sealed: "sealed".to_string(),
                key_fingerprint: "fp".to_string(),
                target_pubkey: "target".to_string(),
            },
        })
        .await
        .expect("submit wrapped key");

    let mut shapes = serde_json::Map::new();
    for (name, uri) in [
        ("status", "/api/status"),
        ("state", "/api/state"),
        ("identity", "/auth/me"),
        ("pendingConfig", "/api/config/pending"),
        ("clusterConfig", "/api/config"),
        ("cloudflareTokenStatus", "/api/cloudflare/token"),
        ("oidcClientSecretStatus", "/api/oidc/client-secret"),
        ("clusterKeyStatus", "/api/secrets/key"),
        ("selfUpdateGithubToken", "/api/self-update/github-token"),
        ("selfUpdateRepo", "/api/self-update/repo"),
        ("selfUpdateRun", "/api/self-update/run"),
        ("drives", "/api/drives"),
    ] {
        let resp = h
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("authorization", format!("Bearer {}", h.admin_key))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri} did not answer 200");
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{uri} is not json: {e}"));
        shapes.insert(name.to_string(), value);
    }

    // The harness runs in a fresh tempdir and the state version moves as the
    // harness sets itself up, so the values are not stable across runs. Only
    // the *shape* is the contract, so the volatile ones are blanked rather
    // than making the test flaky.
    let mut shapes = serde_json::Value::Object(shapes);
    let tempdir = h.state.config_path.parent().unwrap().to_string_lossy().to_string();
    normalize(&mut shapes, &tempdir);

    let rendered = serde_json::to_string_pretty(&shapes).unwrap() + "\n";
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(API_SHAPES_FIXTURE);

    if std::env::var("UPDATE_API_FIXTURES").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).expect("create fixture dir");
        std::fs::write(&path, &rendered).expect("write fixture");
        return;
    }

    let existing = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}\nregenerate with: UPDATE_API_FIXTURES=1 cargo test -p cl-api api_shapes",
            path.display()
        )
    });

    // Compare parsed, so formatting or key order never fails the test.
    let expected: serde_json::Value = serde_json::from_str(&existing).expect("fixture is json");
    let actual: serde_json::Value = serde_json::from_str(&rendered).expect("rendered is json");
    assert_eq!(
        actual, expected,
        "the api response shape changed.\nIf that was intended, regenerate the fixture the ui \
         typechecks against:\n    UPDATE_API_FIXTURES=1 cargo test -p cl-api api_shapes\n\
         and run the ui tests, which will fail if a TypeScript interface no longer matches."
    );
}
