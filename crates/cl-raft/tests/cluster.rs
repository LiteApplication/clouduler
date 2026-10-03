//! In-process multi node tests. Real sockets on loopback, real redb files, so
//! everything except the machine boundary is exercised.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use cl_raft::ids::node_id;
use cl_raft::node::{ClusterNode, Options};
use cl_raft::state::{Availability, NodeState, Request, Role};
use openraft::BasicNode;
use tempfile::TempDir;

/// Grab a free port by binding and immediately dropping the listener. Racy in
/// principle, fine in practice and much simpler than plumbing port 0 through.
fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr")
}

/// Deterministic per-name id, purely so these tests can derive the same id
/// from a name twice without threading a `Uuid` through everything.
fn test_id(name: &str) -> uuid::Uuid {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, name.as_bytes())
}

struct Harness {
    _dir: TempDir,
    nodes: Vec<ClusterNode>,
    addrs: Vec<SocketAddr>,
}

impl Harness {
    /// Start `count` nodes and form a cluster with all of them voting.
    async fn start(count: usize) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let mut nodes = Vec::new();
        let mut addrs = Vec::new();

        for i in 0..count {
            let addr = free_addr();
            let name = format!("n{i}");
            let node = ClusterNode::start(Options {
                name: name.clone(),
                id: test_id(&name),
                rpc_addr: addr,
                data_dir: PathBuf::from(dir.path()).join(&name),
                cluster_name: "test".into(),
            })
            .await
            .expect("start node");
            nodes.push(node);
            addrs.push(addr);
        }

        let members: BTreeMap<_, _> = (0..count)
            .map(|i| {
                (
                    node_id(test_id(&format!("n{i}"))),
                    BasicNode::new(addrs[i].to_string()),
                )
            })
            .collect();
        nodes[0].initialize(members).await.expect("initialize");

        Self {
            _dir: dir,
            nodes,
            addrs,
        }
    }

    /// Index of the current leader, waiting for one to appear.
    async fn leader(&self) -> usize {
        for _ in 0..100 {
            if let Some(i) = self.nodes.iter().position(|n| n.is_leader()) {
                return i;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("no leader elected");
    }

    /// Wait until every node has applied at least `version` commands.
    async fn await_version(&self, version: u64) {
        for _ in 0..100 {
            let mut all = true;
            for node in &self.nodes {
                if node.state().await.version < version {
                    all = false;
                    break;
                }
            }
            if all {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("state did not converge to version {version}");
    }
}

fn node_state(name: &str, addr: SocketAddr) -> NodeState {
    NodeState {
        name: name.to_string(),
        id: test_id(name),
        role: Role::Manager,
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
        node_pubkey: Default::default(),
        memory_total_bytes: Default::default(),
    }
}

#[tokio::test]
async fn three_nodes_elect_a_leader_and_replicate() {
    let cluster = Harness::start(3).await;
    let leader = cluster.leader().await;

    let response = cluster.nodes[leader]
        .submit(Request::UpsertNode(node_state("n0", cluster.addrs[0])))
        .await
        .expect("submit");
    assert!(response.is_ok());

    cluster.await_version(1).await;
    for node in &cluster.nodes {
        assert!(node.state().await.nodes.contains_key("n0"));
    }
}

#[tokio::test]
async fn followers_forward_writes_to_the_leader() {
    let cluster = Harness::start(3).await;
    let leader = cluster.leader().await;
    let follower = (leader + 1) % 3;
    assert!(!cluster.nodes[follower].is_leader());

    // Submitting on a follower must still land, via the forwarding path.
    let response = cluster.nodes[follower]
        .submit(Request::UpsertNode(node_state("n1", cluster.addrs[1])))
        .await
        .expect("forwarded submit");
    assert!(response.is_ok());

    cluster.await_version(1).await;
    assert!(cluster.nodes[leader].state().await.nodes.contains_key("n1"));
}

#[tokio::test]
async fn rejected_commands_report_their_reason() {
    let cluster = Harness::start(3).await;
    let leader = cluster.leader().await;

    let response = cluster.nodes[leader]
        .submit(Request::SetAvailability {
            node: "ghost".into(),
            availability: Availability::Drain,
        })
        .await
        .expect("submit");
    assert_eq!(response.error.as_deref(), Some("unknown node ghost"));
}

#[tokio::test]
async fn every_node_reports_quorum_once_formed() {
    let cluster = Harness::start(3).await;
    cluster.leader().await;

    // Give the followers a heartbeat to learn who leads.
    tokio::time::sleep(Duration::from_millis(600)).await;
    for node in &cluster.nodes {
        let status = node.quorum().status();
        assert!(status.has_quorum, "{} lacks quorum", node.name);
        assert_eq!(status.voters, 3);
        assert_eq!(status.needed(), 2);
    }
}

#[tokio::test]
async fn a_worker_joins_as_a_non_voter() {
    let cluster = Harness::start(3).await;
    let leader = cluster.leader().await;

    let dir = TempDir::new().expect("temp dir");
    let addr = free_addr();
    let worker = ClusterNode::start(Options {
        name: "w0".into(),
        id: test_id("w0"),
        rpc_addr: addr,
        data_dir: dir.path().join("w0"),
        cluster_name: "test".into(),
    })
    .await
    .expect("start worker");

    cluster.nodes[leader]
        .add_worker(worker.id, addr)
        .await
        .expect("add worker");

    cluster.nodes[leader]
        .submit(Request::UpsertNode(node_state("n0", cluster.addrs[0])))
        .await
        .expect("submit");

    // The worker replicates state so it can reconcile locally, but it must not
    // count towards the quorum.
    for _ in 0..100 {
        if worker.state().await.nodes.contains_key("n0") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(worker.state().await.nodes.contains_key("n0"));
    assert_eq!(worker.quorum().status().voters, 3);
}

#[tokio::test]
async fn a_single_node_cluster_leads_itself() {
    let cluster = Harness::start(1).await;
    assert_eq!(cluster.leader().await, 0);

    let status = cluster.nodes[0].quorum().status();
    assert!(status.has_quorum);
    assert_eq!(status.needed(), 1);
}

#[tokio::test]
async fn a_node_without_a_cluster_holds_no_quorum() {
    let dir = TempDir::new().expect("temp dir");
    let node = ClusterNode::start(Options {
        name: "lonely".into(),
        id: test_id("lonely"),
        rpc_addr: free_addr(),
        data_dir: dir.path().join("lonely"),
        cluster_name: "test".into(),
    })
    .await
    .expect("start node");

    // Uninitialized, so nothing may be scheduled here.
    assert!(!node.quorum().has_quorum());
    assert!(node.submit(Request::NoOp).await.is_err());
}

/// A config edit made on any node reaches every node, which is what lets each
/// daemon write its own copy of the file.
#[tokio::test]
async fn a_config_edit_replicates_to_every_node() {
    let cluster = Harness::start(3).await;
    let leader = cluster.leader().await;
    let follower = (leader + 1) % 3;

    let yaml = "cluster:\n  name: home\n";
    let response = cluster.nodes[follower]
        .submit(Request::PutClusterConfig {
            yaml: yaml.to_string(),
            expect_generation: 0,
        })
        .await
        .expect("submit");
    assert!(response.is_ok(), "{:?}", response.error);

    cluster.await_version(1).await;
    for node in &cluster.nodes {
        let stored = node
            .state()
            .await
            .cluster_config
            .expect("config replicated");
        assert_eq!(stored.yaml, yaml);
        assert_eq!(stored.generation, 1);
    }
}
