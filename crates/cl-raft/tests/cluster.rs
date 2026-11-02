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
    keys: cl_raft::KeyWatch,
}

impl Harness {
    /// Start `count` nodes and form a cluster with all of them voting.
    async fn start(count: usize) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let keys = cl_raft::throwaway_key();
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
                keys: keys.clone(),
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
            keys,
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
        version: Default::default(),
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
        keys: cluster.keys.clone(),
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
async fn a_leader_marked_unreachable_by_a_past_leader_is_cleared() {
    let mut cluster = Harness::start(1).await;
    assert_eq!(cluster.leader().await, 0);
    let node = std::sync::Arc::new(cluster.nodes.remove(0));

    // What a previous leader leaves behind when this node was down while it
    // probed, and this node then wins the election.
    for request in [
        Request::UpsertNode(node_state("n0", cluster.addrs[0])),
        Request::SetReachable {
            node: "n0".into(),
            reachable: false,
        },
    ] {
        assert!(node.submit(request).await.expect("submit").is_ok());
    }
    assert!(node.state().await.unreachable.contains("n0"));

    tokio::spawn(
        node.clone()
            .run_failure_detector(Duration::from_millis(100), Duration::from_secs(5)),
    );

    for _ in 0..50 {
        if !node.state().await.unreachable.contains("n0") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the leader stayed flagged unreachable");
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
        keys: cl_raft::throwaway_key(),
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

#[tokio::test]
async fn a_peer_holding_the_cluster_key_can_talk_to_a_node() {
    let cluster = Harness::start(1).await;
    let peer = cl_raft::network::Peer::connect(&cluster.addrs[0].to_string(), cluster.keys.clone());
    assert!(cl_raft::network::ping(&peer).await);
}

#[tokio::test]
async fn a_peer_with_the_wrong_key_is_refused() {
    let cluster = Harness::start(1).await;
    let peer =
        cl_raft::network::Peer::connect(&cluster.addrs[0].to_string(), cl_raft::throwaway_key());
    assert!(!cl_raft::network::ping(&peer).await);
}

/// The attack the handshake exists for: a bare socket that skips it and goes
/// straight to a framed request, here one that would write cluster state.
#[tokio::test]
async fn a_raw_connection_cannot_submit_a_write_without_authenticating() {
    use futures::{SinkExt, StreamExt};
    use tokio::io::AsyncReadExt;

    let cluster = Harness::start(1).await;
    let leader = cluster.leader().await;
    let before = cluster.nodes[leader].state().await.version;

    let mut stream = tokio::net::TcpStream::connect(cluster.addrs[0])
        .await
        .expect("connect");
    // Swallow the server's challenge, then ignore it and speak rpc anyway.
    let mut hello = [0u8; 36];
    stream.read_exact(&mut hello).await.expect("challenge");
    let mut conn = cl_raft::rpc::frame(stream);
    let request = cl_raft::rpc::RpcRequest::Forward(Request::PutConfig {
        name: "evil".into(),
        content: "x".into(),
        owner: None,
    });
    let _ = conn.send(cl_raft::rpc::encode(&request).unwrap()).await;
    // Whatever comes back, it must not be a decodable response.
    let reply = tokio::time::timeout(Duration::from_secs(2), conn.next()).await;
    assert!(
        !matches!(reply, Ok(Some(Ok(_)))),
        "an unauthenticated request got an answer"
    );
    assert_eq!(cluster.nodes[leader].state().await.version, before);
}

fn id_of(i: usize) -> cl_raft::ids::NodeId {
    node_id(test_id(&format!("n{i}")))
}

/// Move `index` to a fresh address through the leader, waiting for the node to
/// have bound it.
async fn move_node(cluster: &Harness, index: usize) -> SocketAddr {
    let new = free_addr();
    cluster.nodes[index].set_rpc_addr(new);
    let leader = cluster.leader().await;
    for _ in 0..50 {
        if cluster.nodes[leader]
            .readdress(id_of(index), new)
            .await
            .is_ok()
        {
            return new;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("could not move n{index} to {new}");
}

#[tokio::test]
async fn a_voter_can_be_moved_to_a_new_address_without_losing_quorum() {
    let cluster = Harness::start(3).await;
    let leader = cluster.leader().await;
    let follower = (leader + 1) % 3;

    let new = move_node(&cluster, follower).await;

    // Everyone, the moved node included, now holds the new address, and the
    // same three nodes still vote.
    for node in &cluster.nodes {
        for _ in 0..50 {
            if node.member_addr(id_of(follower)) == Some(new.to_string()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(node.member_addr(id_of(follower)), Some(new.to_string()));
        assert!(node.is_voter(id_of(follower)).await);
    }

    // The old address is gone from the picture, yet writes still commit and
    // reach the moved node over its new one.
    let leader = cluster.leader().await;
    let response = cluster.nodes[leader]
        .submit(Request::UpsertNode(node_state("n0", cluster.addrs[0])))
        .await
        .expect("submit");
    assert!(response.is_ok());
    cluster.await_version(1).await;
}

#[tokio::test]
async fn the_leader_can_move_itself() {
    let cluster = Harness::start(3).await;
    let leader = cluster.leader().await;

    let new = move_node(&cluster, leader).await;

    for node in &cluster.nodes {
        for _ in 0..50 {
            if node.member_addr(id_of(leader)) == Some(new.to_string()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(node.member_addr(id_of(leader)), Some(new.to_string()));
    }

    // Writes sent to a follower are forwarded to the leader at its new address.
    let follower = (leader + 1) % 3;
    let response = cluster.nodes[follower]
        .submit(Request::UpsertNode(node_state("n0", cluster.addrs[0])))
        .await
        .expect("submit");
    assert!(response.is_ok());
    cluster.await_version(1).await;
}

#[tokio::test]
async fn an_address_nobody_answers_on_is_refused_and_membership_is_untouched() {
    let cluster = Harness::start(3).await;
    let leader = cluster.leader().await;
    let follower = (leader + 1) % 3;

    let nowhere = free_addr();
    let result = cluster.nodes[leader]
        .readdress(id_of(follower), nowhere)
        .await;
    assert!(result.is_err(), "moved a node to an address nobody serves");
    assert_eq!(
        cluster.nodes[leader].member_addr(id_of(follower)),
        Some(cluster.addrs[follower].to_string())
    );

    // Still healthy, so nothing is wedged by the refusal.
    let response = cluster.nodes[leader]
        .submit(Request::UpsertNode(node_state("n0", cluster.addrs[0])))
        .await
        .expect("submit");
    assert!(response.is_ok());
}

#[tokio::test]
async fn a_node_cannot_be_moved_onto_another_nodes_address() {
    let cluster = Harness::start(3).await;
    let leader = cluster.leader().await;
    let a = (leader + 1) % 3;
    let b = (leader + 2) % 3;

    let result = cluster.nodes[leader]
        .readdress(id_of(a), cluster.addrs[b])
        .await;
    assert!(result.is_err());
    assert_eq!(
        cluster.nodes[leader].member_addr(id_of(a)),
        Some(cluster.addrs[a].to_string())
    );
}

#[tokio::test]
async fn a_node_moved_and_moved_back_still_works() {
    let cluster = Harness::start(3).await;
    let leader = cluster.leader().await;
    let follower = (leader + 1) % 3;

    move_node(&cluster, follower).await;
    // Editing the config back: the original address is served again at once
    // because it was never closed.
    cluster.nodes[follower].set_rpc_addr(cluster.addrs[follower]);
    let leader = cluster.leader().await;
    cluster.nodes[leader]
        .readdress(id_of(follower), cluster.addrs[follower])
        .await
        .expect("move back");
    assert_eq!(
        cluster.nodes[leader].member_addr(id_of(follower)),
        Some(cluster.addrs[follower].to_string())
    );
}
