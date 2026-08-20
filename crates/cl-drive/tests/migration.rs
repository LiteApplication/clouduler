//! Migration handoff tests: a real single-node raft cluster (so
//! `MigrationPhase` transitions genuinely go through `apply`), a fake
//! `InstanceControl` (no docker), and a real loopback transport between two
//! temp directories standing in for the source and target volume roots.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use cl_drive::migration::{run, InstanceControl, MigrationJob};
use cl_drive::{BoxFuture, DeltaEngine, DriveRegistry, Result as DriveResult};
use cl_raft::ids::node_id;
use cl_raft::node::{ClusterNode, Options};
use cl_raft::state::{Availability, MigrationPhase, NodeState, Request, Role};
use cl_raft::InstanceId;
use openraft::BasicNode;
use tempfile::TempDir;

fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr")
}

/// Records calls in order, so tests can assert the phase sequence, and can
/// be told to fail on demand to exercise the failure path.
#[derive(Default)]
struct RecordingControl {
    events: Mutex<Vec<String>>,
    fail_stop: AtomicUsize,
}

impl RecordingControl {
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

impl InstanceControl for RecordingControl {
    fn stop_instance<'a>(&'a self, instance: &'a InstanceId, node: &'a str) -> BoxFuture<'a, DriveResult<()>> {
        Box::pin(async move {
            self.events.lock().unwrap().push(format!("stop:{instance}@{node}"));
            if self.fail_stop.load(Ordering::SeqCst) > 0 {
                return Err(cl_drive::DriveError::Transfer { message: "forced stop failure".into() });
            }
            Ok(())
        })
    }

    fn start_instance<'a>(&'a self, instance: &'a InstanceId, node: &'a str) -> BoxFuture<'a, DriveResult<()>> {
        Box::pin(async move {
            self.events.lock().unwrap().push(format!("start:{instance}@{node}"));
            Ok(())
        })
    }
}

/// Bring up a solo-node raft cluster, a source and target temp dir each
/// implementing drive "fast", and a listener on the target so migration can
/// reach it over the drive transport.
struct Harness {
    _dir: TempDir,
    node: ClusterNode,
    source: TempDir,
    target: TempDir,
    target_addr: SocketAddr,
}

impl Harness {
    async fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let addr = free_addr();
        let node = ClusterNode::start(Options {
            name: "n1".into(),
            rpc_addr: addr,
            data_dir: dir.path().join("n1"),
            cluster_name: "test".into(),
        })
        .await
        .unwrap();
        node.initialize(BTreeMap::from([(node_id("n1"), BasicNode::new(addr.to_string()))]))
            .await
            .unwrap();

        // Wait for the solo node to elect itself leader.
        for _ in 0..100 {
            if node.is_leader() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(node.is_leader(), "solo node never became leader");

        let source = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        let target_addr = free_addr();

        let target_registry = Arc::new(DriveRegistry::from_drives(
            "target",
            BTreeMap::from([("fast".to_string(), target.path().to_path_buf())]),
        ));
        let listener = tokio::net::TcpListener::bind(target_addr).await.unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                let registry = target_registry.clone();
                tokio::spawn(async move {
                    let _ = cl_drive::transport::receive(stream, &registry).await;
                });
            }
        });

        node.submit(Request::UpsertNode(NodeState {
            name: "source".into(),
            role: Role::Manager,
            rpc_addr: free_addr(),
            drives: BTreeMap::from([("fast".to_string(), PathBuf::from("/unused"))]),
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
        .unwrap();
        node.submit(Request::UpsertNode(NodeState {
            name: "target".into(),
            role: Role::Manager,
            rpc_addr: target_addr,
            drives: BTreeMap::from([("fast".to_string(), PathBuf::from("/unused"))]),
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
        .unwrap();

        Self { _dir: dir, node, source, target, target_addr }
    }

    fn source_registry(&self) -> DriveRegistry {
        DriveRegistry::from_drives(
            "source",
            BTreeMap::from([("fast".to_string(), self.source.path().to_path_buf())]),
        )
    }
}

fn instance() -> InstanceId {
    InstanceId::new("gitlab", "web", 0)
}

fn volume() -> cl_raft::VolumeId {
    cl_raft::VolumeId::new("fast", "data")
}

async fn begin_migration(node: &ClusterNode) {
    node.submit(Request::Assign {
        instance: instance(),
        node: "source".into(),
        drives: BTreeSet::from(["fast".to_string()]),
    })
    .await
    .unwrap();
    node.submit(Request::UpsertVolume { id: volume(), primary: "source".into() })
        .await
        .unwrap();
    node.submit(Request::BeginMigration {
        instance: instance(),
        to: "target".into(),
        drives: BTreeSet::from(["fast".to_string()]),
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn migration_runs_every_phase_in_order_and_moves_the_data() {
    let harness = Harness::start().await;
    let source_volume = harness.source.path().join("data");
    std::fs::create_dir_all(&source_volume).unwrap();
    std::fs::write(source_volume.join("a.txt"), b"payload").unwrap();

    begin_migration(&harness.node).await;

    let control = RecordingControl::default();
    let engine = DeltaEngine::default();
    let registry = harness.source_registry();
    let job = MigrationJob {
        instance: instance(),
        source_node: "source".into(),
        target_node: "target".into(),
        target_addr: harness.target_addr,
        volumes: vec![volume()],
        registry: &registry,
        engine: &engine,
        control: &control,
    };

    run(&harness.node, &job).await.expect("migration succeeds");

    assert_eq!(
        control.events(),
        vec!["stop:gitlab.web.0@source".to_string(), "start:gitlab.web.0@target".to_string()]
    );

    let state = harness.node.state().await;
    let assignment = &state.assignments[&instance()];
    assert_eq!(assignment.node, "target");
    assert!(assignment.migration.is_none(), "a finished migration clears its record");
    assert_eq!(state.volumes[&volume()].primary, "target");

    assert_eq!(std::fs::read(harness.target.path().join("data/a.txt")).unwrap(), b"payload");
}

#[tokio::test]
async fn a_failure_before_switching_leaves_the_source_authoritative() {
    let harness = Harness::start().await;
    begin_migration(&harness.node).await;

    let control = RecordingControl::default();
    control.fail_stop.store(1, Ordering::SeqCst);
    let engine = DeltaEngine::default();
    let registry = harness.source_registry();
    let job = MigrationJob {
        instance: instance(),
        source_node: "source".into(),
        target_node: "target".into(),
        target_addr: harness.target_addr,
        volumes: vec![volume()],
        registry: &registry,
        engine: &engine,
        control: &control,
    };

    let err = run(&harness.node, &job).await.expect_err("stop failure should abort the migration");
    assert!(err.to_string().contains("forced stop failure"));

    let state = harness.node.state().await;
    let assignment = &state.assignments[&instance()];
    // Source keeps the instance and stays the volume's primary.
    assert_eq!(assignment.node, "source");
    assert_eq!(state.volumes[&volume()].primary, "source");
    match &assignment.migration.as_ref().unwrap().phase {
        MigrationPhase::Failed { reason } => assert!(reason.contains("forced stop failure")),
        other => panic!("expected Failed, got {other:?}"),
    }
}
