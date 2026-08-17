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
            port: 443,
        },
        data_dir: dir.path().to_path_buf(),
        failover: Default::default(),
        secrets: Default::default(),
        auth: Default::default(),
        cloudflare: None,
    };

    let state = AppState {
        cluster: Arc::new(node),
        docker: Docker::connect().expect("docker client construction does not dial the socket"),
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
    assert!(value.get("is_leader").is_some());
    assert!(value.get("has_quorum").is_some());
    assert!(value.get("version").is_some());
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
