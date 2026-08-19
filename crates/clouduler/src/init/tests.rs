//! The join, end to end: a real api server over real TLS, and init's client
//! walking the whole exchange against it.
//!
//! Everything below the http boundary is exercised for real, because the parts
//! that break here are exactly the ones a unit test cannot see: the pinned
//! certificate verifier, the auth header, the config round trip through serde,
//! and the edit landing in raft rather than only on one node's disk.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use cl_api::auth::keys;
use cl_config::{ClusterConfig, NodeRole};
use cl_raft::node::{ClusterNode, Options};
use cl_raft::state::{Availability, NodeState, Role as ClusterRole};
use openraft::BasicNode;
use tempfile::TempDir;

use super::plan::{self, ClusterPlan, NodePlan};

fn ip(text: &str) -> IpAddr {
    text.parse().expect("ip")
}

fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr")
}

fn founding_config() -> ClusterConfig {
    plan::new_cluster(&ClusterPlan {
        cluster_name: "home".into(),
        subnet: "10.10.0.0/24".parse().unwrap(),
        internal_domain: Some("int.example.com".into()),
        web_address: "cloud".into(),
        web_port: 8443,
        drives: BTreeMap::new(),
        node: NodePlan::new("n1", NodeRole::Manager, ip("10.10.0.1")),
    })
    .expect("valid founding config")
}

struct Server {
    /// `host:port` the api answers on.
    address: String,
    token: String,
    cluster: Arc<ClusterNode>,
    fingerprint: String,
    _dir: TempDir,
}

/// Stand up one node's api on loopback with a self signed certificate, exactly
/// as a freshly initialised cluster serves it.
async fn serve(config: &ClusterConfig) -> Server {
    // main does this at startup; tests never run main.
    crate::install_crypto_provider();

    let dir = TempDir::new().expect("tempdir");
    let rpc_addr = free_addr();
    let cluster = ClusterNode::start(Options {
        name: "n1".into(),
        rpc_addr,
        data_dir: dir.path().join("raft"),
        cluster_name: config.cluster.name.clone(),
    })
    .await
    .expect("start node");

    cluster
        .initialize(BTreeMap::from([(
            cl_raft::node_id("n1"),
            BasicNode::new(rpc_addr.to_string()),
        )]))
        .await
        .expect("initialize");
    for _ in 0..200 {
        if cluster.is_leader() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(cluster.is_leader(), "a single node cluster leads itself");

    cluster
        .submit(cl_raft::Request::UpsertNode(NodeState {
            name: "n1".into(),
            role: ClusterRole::Manager,
            rpc_addr,
            drives: BTreeMap::new(),
            availability: Availability::Active,
            labels: Default::default(),
            egress_networks: Default::default(),
            networks: Default::default(),
            open_ports: Default::default(),
            arch: Default::default(),
            binary_hash: Default::default(),
        }))
        .await
        .expect("seed roster");

    // Seeded before the server starts, so its bootstrap key is never minted and
    // this is the only key that works.
    let new_key = keys::generate(Some("test".into()), 0);
    let token = new_key.secret.clone();
    let store = keys::ApiKeyStore {
        keys: vec![new_key.record],
    };
    cluster
        .submit(cl_raft::Request::PutConfig {
            name: keys::API_KEYS_CONFIG.to_string(),
            content: store.encode(),
            owner: None,
        })
        .await
        .expect("seed api key");

    let cert = rcgen::generate_simple_self_signed(vec![config.web_fqdn()]).expect("cert");
    let fingerprint = super::join::fingerprint(cert.cert.der());
    let listen_addr = free_addr();

    let cluster = Arc::new(cluster);
    let server = cl_api::ApiServer::new(cl_api::ApiServerOptions {
        cluster: cluster.clone(),
        docker: std::sync::Arc::new(tokio::sync::RwLock::new(cl_docker::Docker::connect().ok())),
        node_name: "n1".into(),
        config_path: dir.path().join("cluster.yaml"),
        cluster_config: config.clone(),
        key_path: dir.path().join("secrets.key"),
        key: None,
        tls_cert_pem: cert.cert.pem().into_bytes(),
        tls_key_pem: cert.signing_key.serialize_pem().into_bytes(),
        listen_addr,
        // This harness drives the join flow over TLS; the proxy's plain
        // backend listener plays no part in it.
        plain_listen_addr: None,
        self_update_artifact_path: dir.path().join("self-update-artifact"),
        shutdown: tokio::sync::watch::channel(()).0,
        http: reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .expect("building the test http client"),
    })
    .await
    .expect("build api");

    tokio::spawn(async move {
        let _ = server.serve().await;
    });
    wait_until_listening(listen_addr).await;

    Server {
        address: listen_addr.to_string(),
        token,
        cluster,
        fingerprint,
        _dir: dir,
    }
}

async fn wait_until_listening(addr: SocketAddr) {
    for _ in 0..200 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the api never started listening on {addr}");
}

#[tokio::test]
async fn a_node_fetches_the_config_adds_itself_and_the_cluster_keeps_it() {
    let server = serve(&founding_config()).await;
    let peer = super::join::Peer::connect(&server.address, server.token.clone(), None)
        .expect("client");

    let fetched = peer.fetch_config().await.expect("fetch");
    assert_eq!(fetched.cluster.name, "home");
    assert_eq!(fetched.nodes.len(), 1);

    let mut joining = NodePlan::new("n2", NodeRole::Worker, ip("10.10.0.2"));
    joining.generate_secret_key = Some(false);
    let amended = plan::add_node(&fetched, &joining, false).expect("amended");
    peer.push_config(&amended).await.expect("push");

    // Back through the api, so this is what the cluster kept rather than what
    // we sent.
    let after = peer.fetch_config().await.expect("refetch");
    assert_eq!(after.nodes.len(), 2);
    let n2 = after.node("n2").expect("n2 is in the cluster now");
    assert_eq!(n2.role, NodeRole::Worker);
    assert_eq!(after.internal_ip(n2), Some(ip("10.10.0.2")));
    assert_eq!(n2.generate_secret_key, Some(false));
}

/// The point of replicating the config: the joining node's entry has to reach
/// every node's copy, not just the one that answered the request.
#[tokio::test]
async fn the_pushed_config_lands_in_the_replicated_state() {
    let server = serve(&founding_config()).await;
    let peer = super::join::Peer::connect(&server.address, server.token.clone(), None)
        .expect("client");

    let fetched = peer.fetch_config().await.expect("fetch");
    let joining = NodePlan::new("n2", NodeRole::Worker, ip("10.10.0.2"));
    let amended = plan::add_node(&fetched, &joining, false).expect("amended");
    peer.push_config(&amended).await.expect("push");

    let stored = server
        .cluster
        .state()
        .await
        .cluster_config
        .expect("the config is in the replicated state");
    let replicated: ClusterConfig = serde_yaml::from_str(&stored.yaml).expect("parses");
    assert!(replicated.node("n2").is_some());
    assert_eq!(stored.generation, 1);
}

/// A confirmed edit must not be undone by the rollback watchdog, or a node
/// would vanish from the cluster a minute after joining it.
#[tokio::test]
async fn the_join_is_confirmed_so_it_does_not_roll_itself_back() {
    let server = serve(&founding_config()).await;
    let peer = super::join::Peer::connect(&server.address, server.token.clone(), None)
        .expect("client");

    let fetched = peer.fetch_config().await.expect("fetch");
    let joining = NodePlan::new("n2", NodeRole::Worker, ip("10.10.0.2"));
    peer.push_config(&plan::add_node(&fetched, &joining, false).expect("amended"))
        .await
        .expect("push");

    // A second edit can only be built on generation 1, which proves the first
    // one was kept rather than rolled back.
    let after = peer.fetch_config().await.expect("refetch");
    let third = NodePlan::new("n3", NodeRole::Worker, ip("10.10.0.3"));
    peer.push_config(&plan::add_node(&after, &third, false).expect("amended"))
        .await
        .expect("second push");

    let final_config = peer.fetch_config().await.expect("refetch");
    assert_eq!(final_config.nodes.len(), 3);
}

#[tokio::test]
async fn a_matching_certificate_pin_is_accepted() {
    let server = serve(&founding_config()).await;
    let peer = super::join::Peer::connect(
        &server.address,
        server.token.clone(),
        Some(server.fingerprint.clone()),
    )
    .expect("client");

    assert!(peer.fetch_config().await.is_ok());
}

#[tokio::test]
async fn a_certificate_that_is_not_the_pinned_one_stops_the_join() {
    let server = serve(&founding_config()).await;
    let wrong = super::join::fingerprint(b"some other certificate");
    let peer =
        super::join::Peer::connect(&server.address, server.token.clone(), Some(wrong)).expect("client");

    assert!(peer.fetch_config().await.is_err());
}

#[tokio::test]
async fn a_bad_api_key_is_reported_as_a_bad_api_key() {
    let server = serve(&founding_config()).await;
    let peer = super::join::Peer::connect(&server.address, "cl_nope_nope".into(), None)
        .expect("client");

    let error = peer.fetch_config().await.unwrap_err().to_string();
    assert!(error.contains("api key"), "{error}");
}
