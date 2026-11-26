pub(crate) use std::collections::{BTreeMap, BTreeSet};
pub(crate) use std::net::SocketAddr;
pub(crate) use std::sync::Arc;
pub(crate) use std::time::Duration;

pub(crate) use axum::body::Body;
pub(crate) use axum::http::{Request, StatusCode};
pub(crate) use axum::Router;
pub(crate) use cl_config::{ClusterConfig, ClusterMeta, Network, NetworkKind, WebConfig};
pub(crate) use cl_docker::Docker;
pub(crate) use cl_raft::node::{ClusterNode, Options};
pub(crate) use cl_raft::state::{Availability, NodeState, Role as ClusterRole};
pub(crate) use cl_secret::ClusterKey;
pub(crate) use openraft::BasicNode;
pub(crate) use tempfile::TempDir;
pub(crate) use tokio::sync::RwLock;
pub(crate) use tower::ServiceExt;

pub(crate) use crate::auth::session::SessionStore;
pub(crate) use crate::auth::{keys, Identity, Role};
pub(crate) use crate::server::{build_router, AppState};

pub(crate) fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr")
}

/// Everything a test needs: the router, plus handles to poke at auth and
/// session state directly rather than through http. `pub(crate)` so other
/// route modules' own test submodules (e.g. `routes::git_backup`) can stand
/// up a real single-node cluster without duplicating this setup.
pub(crate) struct Harness {
    pub(crate) router: Router,
    pub(crate) state: AppState,
    _dir: TempDir,
    pub(crate) admin_key: String,
    pub(crate) n0_id: uuid::Uuid,
}

pub(crate) async fn harness() -> Harness {
    let dir = TempDir::new().expect("tempdir");
    let addr = free_addr();
    let n0_id = uuid::Uuid::new_v4();
    let node = ClusterNode::start(Options {
        name: "n0".into(),
        id: n0_id,
        rpc_addr: addr,
        data_dir: dir.path().join("n0"),
        cluster_name: "test".into(),
        keys: cl_raft::throwaway_key(),
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
    assert!(
        node.is_leader(),
        "single node cluster must elect itself leader"
    );

    node.submit(cl_raft::Request::UpsertNode(NodeState {
        name: "n0".into(),
        id: n0_id,
        role: ClusterRole::Manager,
        rpc_addr: addr,
        drives: BTreeMap::new(),
        availability: Availability::Active,
        labels: Default::default(),
        last_resort: false,
        egress_networks: Default::default(),
        networks: Default::default(),
        open_ports: Default::default(),
        arch: Default::default(),
        binary_hash: Default::default(),
        ui_hash: Default::default(),
        started_at: Default::default(),
        node_pubkey: Default::default(),
        memory_total_bytes: Default::default(),
        gpus: Default::default(),
        images: Default::default(),
        version: Default::default(),
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
        cluster: ClusterMeta {
            name: "test".into(),
        },
        networks: BTreeMap::from([(
            "internal".to_string(),
            Network {
                kind: NetworkKind::Internal,
                subnet: None,
                domain: None,
                publish_dns: true,
                access: None,
                managed: false,
                open_ports: Vec::new(),
                proxy: false,
                shortcut: Default::default(),
                container: None,
            },
        )]),
        drives: BTreeMap::new(),
        nodes: BTreeMap::from([(
            "n0".to_string(),
            cl_config::Node {
                id: n0_id,
                role: cl_config::NodeRole::Manager,
                network: BTreeMap::from([(
                    "internal".to_string(),
                    cl_config::NodeAddress::Ip("127.0.0.1".parse().unwrap()),
                )]),
                drives: BTreeMap::new(),
                labels: BTreeMap::new(),
                rpc_port: cl_config::DEFAULT_RPC_PORT,
                data_dir: None,
                docker_data_dir: None,
                generate_secret_key: None,
                last_resort: false,
                disk_probe: true,
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
        backup: None,
        mcp: cl_config::McpConfig { enabled: true },
        updates: Default::default(),
        terraform: None,
    };

    let node = Arc::new(node);
    let (shutdown, _shutdown_rx) = tokio::sync::watch::channel(());
    let cluster_key = Arc::new(ClusterKey::generate());
    let state = AppState {
        cluster: node.clone(),
        docker: cl_docker::DockerHandle::new(Docker::connect().ok()),
        pulling: Arc::new(RwLock::new(Default::default())),
        link_health: cl_sched::LinkHealth::new(),
        migration_sync: Arc::new(parking_lot::RwLock::new(Default::default())),
        node_name: "n0".to_string(),
        config_path: dir.path().join("cluster.yaml"),
        cluster_config: Arc::new(RwLock::new(cluster_config)),
        key_path: dir.path().join("secrets.key"),
        key: Arc::new(RwLock::new(Some(cluster_key.clone()))),
        key_tx: tokio::sync::watch::channel(Some(cluster_key)).0,
        sessions: Arc::new(SessionStore::new(node.clone())),
        oidc: Arc::new(RwLock::new(None)),
        self_update_artifact_path: dir.path().join("self-update-artifact"),
        data_dir: dir.path().to_path_buf(),
        shutdown,
        http: reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .expect("building the test http client"),
        tls_fingerprint: "test:fingerprint".to_string(),
        proxy_health: cl_proxy::HealthTable::default(),
        nudges: Default::default(),
        git_export_author: Default::default(),
        proxy_routes: Default::default(),
        internal_replay: Default::default(),
        oauth: Default::default(),
        mcp: Default::default(),
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

pub(crate) async fn viewer_cookie(state: &AppState) -> String {
    let id = state
        .sessions
        .create(
            Identity {
                subject: "viewer@example.com".into(),
                username: "viewer@example.com".into(),
                role: Role::Viewer,
            },
            Duration::from_secs(60),
        )
        .await
        .expect("storing the viewer session");
    format!("cl_session={id}")
}

pub(crate) async fn deploy(h: &Harness, name: &str, body: &str) -> StatusCode {
    h.router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/stacks/{name}"))
                .header("authorization", format!("Bearer {}", h.admin_key))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

pub(crate) const PLAIN_SPEC: &str = r#"{"spec":"services:\n  web:\n    image: nginx\n"}"#;

pub(crate) async fn signed_get(h: &Harness, path: &str) -> axum::response::Response {
    let empty = cl_secret::internal_auth::body_sha256_hex(b"");
    let now = cl_secret::internal_auth::unix_now();
    let req = internal_request(h, "GET", path, now, &empty, Vec::new()).await;
    h.router.clone().oneshot(req).await.unwrap()
}

/// A request carrying every header an internal caller sends. `body_hash`
/// is what gets signed and sent in the hash header; `body` is what is actually
/// sent, so a test can make the two disagree.
pub(crate) async fn internal_request(
    h: &Harness,
    method: &str,
    path: &str,
    ts: u64,
    body_hash: &str,
    body: Vec<u8>,
) -> Request<Body> {
    use cl_update::internal_client::*;
    let key_guard = h.state.key.read().await;
    let key = key_guard.as_ref().expect("test cluster holds a key");
    let nonce = cl_secret::internal_auth::new_nonce();
    let sig = cl_secret::internal_auth::sign_v3(key, method, path, ts, &nonce, body_hash);
    Request::builder()
        .method(method)
        .uri(path)
        .header(TIMESTAMP_HEADER, ts.to_string())
        .header(BODY_SHA256_HEADER, body_hash)
        .header(NONCE_HEADER, nonce)
        .header(SIGNATURE_V3_HEADER, sig)
        .body(Body::from(body))
        .unwrap()
}
