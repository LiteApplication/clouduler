//! Route level tests: a real single node raft cluster, a lazily connected
//! docker client (never actually dialed, since none of the routes exercised
//! here touch it), driven through the router with `tower::ServiceExt::oneshot`.

use std::collections::BTreeMap;
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
}

async fn harness() -> Harness {
    let dir = TempDir::new().expect("tempdir");
    let addr = free_addr();
    let node = ClusterNode::start(Options {
        name: "n0".into(),
        rpc_addr: addr,
        data_dir: dir.path().join("n0"),
        cluster_name: "test".into(),
    })
    .await
    .expect("start node");

    let members = BTreeMap::from([(cl_raft::node_id("n0"), BasicNode::new(addr.to_string()))]);
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
        role: ClusterRole::Manager,
        rpc_addr: addr,
        drives: BTreeMap::new(),
        availability: Availability::Active,
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
        nodes: BTreeMap::new(),
        web: WebConfig {
            address: "cloud".into(),
            network: "internal".into(),
            port: 8443,
        },
        data_dir: dir.path().to_path_buf(),
        failover: Default::default(),
        secrets: Default::default(),
        auth: Default::default(),
        cloudflare: None,
        proxy: Default::default(),
    };

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
    };

    let router = build_router(state.clone());
    Harness {
        router,
        state,
        _dir: dir,
        admin_key: new_key.secret,
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

/// The token is write-only: the status route says whether one exists and where,
/// never what it is.
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

#[tokio::test]
async fn setting_a_cloudflare_token_without_cloudflare_enabled_is_a_conflict() {
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
    assert_eq!(resp.status(), StatusCode::CONFLICT);
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
            role: ClusterRole::Worker,
            rpc_addr: free_addr(),
            drives: BTreeMap::new(),
            availability: Availability::Active,
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
