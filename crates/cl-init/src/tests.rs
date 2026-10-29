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
/// as a freshly initialised cluster serves it. `key` stands in for whatever
/// this node already holds on disk, `None` matching a fresh, keyless node.
async fn serve(config: &ClusterConfig, key: Option<cl_secret::ClusterKey>) -> Server {
    // main does this at startup; tests never run main.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let n1_id = config.node("n1").expect("founding node in config").id;
    let dir = TempDir::new().expect("tempdir");
    let rpc_addr = free_addr();
    let cluster = ClusterNode::start(Options {
        name: "n1".into(),
        id: n1_id,
        rpc_addr,
        data_dir: dir.path().join("raft"),
        cluster_name: config.cluster.name.clone(),
        keys: cl_raft::throwaway_key(),
    })
    .await
    .expect("start node");

    cluster
        .initialize(BTreeMap::from([(
            cl_raft::node_id(n1_id),
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
            id: n1_id,
            role: ClusterRole::Manager,
            rpc_addr,
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
            version: Default::default(),
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
        pulling: std::sync::Arc::new(tokio::sync::RwLock::new(Default::default())),
        migration_sync: std::sync::Arc::new(std::sync::RwLock::new(Default::default())),
        node_name: "n1".into(),
        config_path: dir.path().join("cluster.yaml"),
        cluster_config: config.clone(),
        key_path: dir.path().join("secrets.key"),
        key: key.map(std::sync::Arc::new),
        key_tx: tokio::sync::watch::channel(None).0,
        tls_cert_pem: cert.cert.pem().into_bytes(),
        tls_key_pem: cert.signing_key.serialize_pem().into_bytes(),
        listen_addr,
        // This harness drives the join flow over TLS; the proxy's plain
        // backend listener plays no part in it.
        plain_listen_addr: None,
        self_update_artifact_path: dir.path().join("self-update-artifact"),
        data_dir: dir.path().to_path_buf(),
        shutdown: tokio::sync::watch::channel(()).0,
        http: reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .expect("building the test http client"),
        node_keypair: None,
        proxy_health: cl_proxy::HealthTable::default(),
        nudges: Default::default(),
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
    let server = serve(&founding_config(), None).await;
    let peer =
        super::join::Peer::connect(&server.address, server.token.clone(), None).expect("client");

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
    let server = serve(&founding_config(), None).await;
    let peer =
        super::join::Peer::connect(&server.address, server.token.clone(), None).expect("client");

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
    let server = serve(&founding_config(), None).await;
    let peer =
        super::join::Peer::connect(&server.address, server.token.clone(), None).expect("client");

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
    let server = serve(&founding_config(), None).await;
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
    let server = serve(&founding_config(), None).await;
    let wrong = super::join::fingerprint(b"some other certificate");
    let peer = super::join::Peer::connect(&server.address, server.token.clone(), Some(wrong))
        .expect("client");

    assert!(peer.fetch_config().await.is_err());
}

#[tokio::test]
async fn a_bad_api_key_is_reported_as_a_bad_api_key() {
    let server = serve(&founding_config(), None).await;
    let peer =
        super::join::Peer::connect(&server.address, "cl_nope_nope".into(), None).expect("client");

    let error = peer.fetch_config().await.unwrap_err().to_string();
    assert!(error.contains("api key"), "{error}");
}

/// The whole point of the per-node keypair scheme: a joining node ends up
/// with a working cluster key without anyone copying a file by hand.
///
/// n1 holds the plaintext key from the start (standing in for a founding
/// node). n2 is represented only at the raft level; it publishes a real
/// keypair's public half, exactly as `report_self` would after `n2` actually
/// joined and started, and a second `ApiServer` is stood up against that
/// same shared cluster with `n2`'s private half and no key on disk, standing
/// in for n2's own daemon. Both servers' background reconciliation loops
/// (`spawn_wrap_for_new_nodes` on n1, `spawn_unwrap_own_key` on n2) are real,
/// not mocked, and this proves they actually meet in the middle through raft.
#[tokio::test]
async fn a_joining_node_receives_the_cluster_key_automatically_with_no_manual_step() {
    let cluster_key = cl_secret::ClusterKey::generate();
    let expected = cluster_key.encode();
    let server = serve(&founding_config(), Some(cluster_key)).await;

    let n2_addr = free_addr();
    let n2_keypair = cl_secret::NodeKeypair::generate();
    server
        .cluster
        .submit(cl_raft::Request::UpsertNode(NodeState {
            name: "n2".into(),
            id: uuid::Uuid::new_v4(),
            role: ClusterRole::Worker,
            rpc_addr: n2_addr,
            drives: BTreeMap::new(),
            availability: Availability::Active,
            labels: Default::default(),
            last_resort: false,
            egress_networks: Default::default(),
            networks: Default::default(),
            open_ports: Default::default(),
            arch: Default::default(),
            binary_hash: Default::default(),
            node_pubkey: Some(n2_keypair.public_key_encoded()),
            memory_total_bytes: Default::default(),
            version: Default::default(),
        }))
        .await
        .expect("seed n2's roster entry with a published pubkey");

    let dir = TempDir::new().expect("tempdir");
    let n2_key_path = dir.path().join("n2-secrets.key");
    let cert = rcgen::generate_simple_self_signed(vec!["n2.example.com".into()]).expect("cert");
    let n2_listen_addr = free_addr();
    let _n2_server = cl_api::ApiServer::new(cl_api::ApiServerOptions {
        cluster: server.cluster.clone(),
        docker: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
        pulling: std::sync::Arc::new(tokio::sync::RwLock::new(Default::default())),
        migration_sync: std::sync::Arc::new(std::sync::RwLock::new(Default::default())),
        node_name: "n2".into(),
        config_path: dir.path().join("cluster.yaml"),
        cluster_config: founding_config(),
        key_path: n2_key_path.clone(),
        key: None,
        key_tx: tokio::sync::watch::channel(None).0,
        tls_cert_pem: cert.cert.pem().into_bytes(),
        tls_key_pem: cert.signing_key.serialize_pem().into_bytes(),
        listen_addr: n2_listen_addr,
        plain_listen_addr: None,
        self_update_artifact_path: dir.path().join("self-update-artifact"),
        data_dir: dir.path().to_path_buf(),
        shutdown: tokio::sync::watch::channel(()).0,
        http: reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .expect("building the test http client"),
        node_keypair: Some(n2_keypair),
        proxy_health: cl_proxy::HealthTable::default(),
        nudges: Default::default(),
    })
    .await
    .expect("build n2's api");

    for _ in 0..200 {
        if n2_key_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let delivered = cl_secret::ClusterKey::load(&n2_key_path).expect("n2 wrote a key file");
    assert_eq!(delivered.encode(), expected);
}

#[tokio::test]
async fn a_keyless_node_fetches_the_cluster_key_wrapped_for_its_own_keypair() {
    let cluster_key = cl_secret::ClusterKey::generate();
    let expected = cluster_key.encode();
    let server = serve(&founding_config(), Some(cluster_key)).await;
    let peer =
        super::join::Peer::connect(&server.address, server.token.clone(), None).expect("client");

    let keypair = cl_secret::NodeKeypair::generate();
    let fetched = peer
        .fetch_cluster_key("n2", &keypair)
        .await
        .expect("fetch the key");
    assert_eq!(fetched.encode(), expected);
}

#[tokio::test]
async fn fetching_the_key_from_a_node_that_holds_none_is_an_error() {
    let server = serve(&founding_config(), None).await;
    let peer =
        super::join::Peer::connect(&server.address, server.token.clone(), None).expect("client");

    let error = peer
        .fetch_cluster_key("n2", &cl_secret::NodeKeypair::generate())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("no cluster key"), "{error}");
}

/// What `clouduler init --yes --join` does for the key: nothing to answer, the
/// key ends up on disk, and the node keypair it was wrapped for sits where the
/// daemon will load the same one.
#[tokio::test]
async fn init_installs_the_fetched_key_without_a_prompt() {
    let cluster_key = cl_secret::ClusterKey::generate();
    let expected = cluster_key.encode();
    let server = serve(&founding_config(), Some(cluster_key)).await;
    let peer =
        super::join::Peer::connect(&server.address, server.token.clone(), None).expect("client");

    let dir = TempDir::new().expect("tempdir");
    let mut config = founding_config();
    config.secrets.key_file = dir.path().join("secrets.key");
    config.secrets.node_key_file = dir.path().join("node.key");
    let mut joining = NodePlan::new("n2", NodeRole::Worker, ip("10.10.0.2"));
    joining.generate_secret_key = Some(false);
    let config = plan::add_node(&config, &joining, false).expect("amended");

    let args = super::InitArgs::parse_from_yes();
    let mut prompt = super::prompt::Prompt::new(std::io::empty(), Vec::new(), false);
    super::setup_secret_key(&mut prompt, &args, &config, "n2", Some(&peer))
        .await
        .expect("setup");

    let installed = cl_secret::ClusterKey::load(&config.secrets.key_file).expect("key installed");
    assert_eq!(installed.encode(), expected);
    assert!(config.secrets.node_key_file.exists());
}

#[tokio::test]
async fn a_provided_key_file_is_installed_instead_of_fetching() {
    let server = serve(&founding_config(), None).await;
    let peer =
        super::join::Peer::connect(&server.address, server.token.clone(), None).expect("client");

    let dir = TempDir::new().expect("tempdir");
    let copy = cl_secret::ClusterKey::generate();
    let copy_path = dir.path().join("copy.key");
    copy.save(&copy_path).expect("write copy");
    let mut config = founding_config();
    config.secrets.key_file = dir.path().join("secrets.key");
    config.secrets.node_key_file = dir.path().join("node.key");
    let mut joining = NodePlan::new("n2", NodeRole::Worker, ip("10.10.0.2"));
    joining.generate_secret_key = Some(false);
    let config = plan::add_node(&config, &joining, false).expect("amended");

    let mut args = super::InitArgs::parse_from_yes();
    args.key_file = Some(copy_path);
    let mut prompt = super::prompt::Prompt::new(std::io::empty(), Vec::new(), false);
    super::setup_secret_key(&mut prompt, &args, &config, "n2", Some(&peer))
        .await
        .expect("setup");

    let installed = cl_secret::ClusterKey::load(&config.secrets.key_file).expect("key installed");
    assert_eq!(installed.encode(), copy.encode());
}
