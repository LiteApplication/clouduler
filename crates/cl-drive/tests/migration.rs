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
    fail_remove: AtomicUsize,
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

    fn remove_instance<'a>(&'a self, instance: &'a InstanceId, node: &'a str) -> BoxFuture<'a, DriveResult<()>> {
        Box::pin(async move {
            self.events.lock().unwrap().push(format!("remove:{instance}@{node}"));
            if self.fail_remove.load(Ordering::SeqCst) > 0 {
                return Err(cl_drive::DriveError::Transfer { message: "forced remove failure".into() });
            }
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
        let n1_id = uuid::Uuid::new_v4();
        let node = ClusterNode::start(Options {
            name: "n1".into(),
            id: n1_id,
            rpc_addr: addr,
            data_dir: dir.path().join("n1"),
            cluster_name: "test".into(),
        })
        .await
        .unwrap();
        node.initialize(BTreeMap::from([(node_id(n1_id), BasicNode::new(addr.to_string()))]))
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
            id: uuid::Uuid::new_v4(),
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
            id: uuid::Uuid::new_v4(),
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

fn instance2() -> InstanceId {
    InstanceId::new("gitlab", "cache", 0)
}

fn volume2() -> cl_raft::VolumeId {
    cl_raft::VolumeId::new("fast", "cache-data")
}

async fn begin_migration(node: &ClusterNode) {
    begin_migration_for(node, instance(), volume()).await;
}

async fn begin_migration_for(node: &ClusterNode, instance: InstanceId, volume: cl_raft::VolumeId) {
    node.submit(Request::Assign {
        instance: instance.clone(),
        node: "source".into(),
        drives: BTreeSet::from(["fast".to_string()]),
    })
    .await
    .unwrap();
    node.submit(Request::UpsertVolume { id: volume, primary: "source".into() })
        .await
        .unwrap();
    node.submit(Request::BeginMigration {
        instance,
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
        registry: Some(&registry),
        engine: &engine,
        control: &control,
        sync_progress: None,
        transfer_gate: Arc::new(tokio::sync::Semaphore::new(1)),
    };

    run(&harness.node, &job).await.expect("migration succeeds");

    assert_eq!(
        control.events(),
        vec![
            "stop:gitlab.web.0@source".to_string(),
            "start:gitlab.web.0@target".to_string(),
            // The source's dead container goes away as part of the move, not
            // whenever its reconcile loop next happens to sweep.
            "remove:gitlab.web.0@source".to_string(),
        ]
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
        registry: Some(&registry),
        engine: &engine,
        control: &control,
        sync_progress: None,
        transfer_gate: Arc::new(tokio::sync::Semaphore::new(1)),
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

/// A stateless instance — no volumes — must be able to migrate off a node
/// that backs no drives at all, and therefore has no `DriveRegistry`.
///
/// Such a node's registry is `None` permanently, not just while the drive
/// subsystem rebuilds. The driver used to require one before claiming any
/// job, so a move like this was skipped on every 5s poll, forever, with no
/// log line: the migration sat in `Stopping` and the instance stayed put on
/// a node it could not even run on. Nothing here needs a registry — it is
/// only ever touched to hand over a volume.
#[tokio::test]
async fn a_volumeless_instance_migrates_off_a_node_with_no_drives() {
    let harness = Harness::start().await;

    harness
        .node
        .submit(Request::Assign {
            instance: instance(),
            node: "source".into(),
            drives: BTreeSet::new(),
        })
        .await
        .unwrap();
    harness
        .node
        .submit(Request::BeginMigration {
            instance: instance(),
            to: "target".into(),
            drives: BTreeSet::new(),
        })
        .await
        .unwrap();

    let control = RecordingControl::default();
    let engine = DeltaEngine::default();
    let job = MigrationJob {
        instance: instance(),
        source_node: "source".into(),
        target_node: "target".into(),
        target_addr: harness.target_addr,
        volumes: Vec::new(),
        registry: None,
        engine: &engine,
        control: &control,
        sync_progress: None,
        transfer_gate: Arc::new(tokio::sync::Semaphore::new(1)),
    };

    run(&harness.node, &job).await.expect("a volumeless migration needs no registry");

    assert_eq!(
        control.events(),
        vec![
            "stop:gitlab.web.0@source".to_string(),
            "start:gitlab.web.0@target".to_string(),
            "remove:gitlab.web.0@source".to_string(),
        ]
    );
    let state = harness.node.state().await;
    let assignment = &state.assignments[&instance()];
    assert_eq!(assignment.node, "target");
    assert!(assignment.migration.is_none(), "a finished migration clears its record");
}

/// Removing the source container is cleanup after the point of no return, so
/// it must not be able to turn a move that actually worked into a `Failed`
/// one. By the time it runs the target owns both the data and the assignment;
/// reporting failure here would invite a retry of a completed migration.
#[tokio::test]
async fn a_source_container_that_will_not_go_away_does_not_fail_the_move() {
    let harness = Harness::start().await;
    let source_volume = harness.source.path().join("data");
    std::fs::create_dir_all(&source_volume).unwrap();
    std::fs::write(source_volume.join("a.txt"), b"payload").unwrap();

    begin_migration(&harness.node).await;

    let control = RecordingControl::default();
    control.fail_remove.store(1, Ordering::SeqCst);
    let engine = DeltaEngine::default();
    let registry = harness.source_registry();
    let job = MigrationJob {
        instance: instance(),
        source_node: "source".into(),
        target_node: "target".into(),
        target_addr: harness.target_addr,
        volumes: vec![volume()],
        registry: Some(&registry),
        engine: &engine,
        control: &control,
        sync_progress: None,
        transfer_gate: Arc::new(tokio::sync::Semaphore::new(1)),
    };

    run(&harness.node, &job).await.expect("a failed cleanup still leaves the move successful");

    let state = harness.node.state().await;
    let assignment = &state.assignments[&instance()];
    assert_eq!(assignment.node, "target");
    assert!(assignment.migration.is_none(), "a finished migration clears its record");
    assert_eq!(state.volumes[&volume()].primary, "target");
}

/// Logs every call into one shared, order-preserving timeline, and can be
/// told to stall inside `start_instance` until told to proceed, or to give
/// that signal once it is done. The tool the test below needs to prove that
/// one job's post-transfer tail (verify/switch/start/remove) does not hold
/// up a second job's stop-and-transfer.
struct GatedControl {
    log: Arc<Mutex<Vec<String>>>,
    label: &'static str,
    block_start_until: Option<Arc<tokio::sync::Notify>>,
    signal_on_remove: Option<Arc<tokio::sync::Notify>>,
}

impl InstanceControl for GatedControl {
    fn stop_instance<'a>(&'a self, _i: &'a InstanceId, _n: &'a str) -> BoxFuture<'a, DriveResult<()>> {
        Box::pin(async move {
            self.log.lock().unwrap().push(format!("stop:{}", self.label));
            Ok(())
        })
    }

    fn start_instance<'a>(&'a self, _i: &'a InstanceId, _n: &'a str) -> BoxFuture<'a, DriveResult<()>> {
        Box::pin(async move {
            if let Some(gate) = &self.block_start_until {
                gate.notified().await;
            }
            self.log.lock().unwrap().push(format!("start:{}", self.label));
            Ok(())
        })
    }

    fn remove_instance<'a>(&'a self, _i: &'a InstanceId, _n: &'a str) -> BoxFuture<'a, DriveResult<()>> {
        Box::pin(async move {
            self.log.lock().unwrap().push(format!("remove:{}", self.label));
            if let Some(gate) = &self.signal_on_remove {
                gate.notify_one();
            }
            Ok(())
        })
    }
}

/// The whole point of `transfer_gate`: two migrations sharing one may not
/// both have their containers down at once, but neither should the first
/// one's tail (verify/switch/start/remove) hold up the second's stop and
/// transfer. Job one is stalled inside `start_instance` — well after it has
/// released the gate at the end of its own `Syncing` — until job two signals
/// that it has finished completely, including its own `start`/`remove`. If
/// job two could only proceed once job one's `start` had already run, this
/// test would deadlock instead of completing.
#[tokio::test]
async fn a_second_migration_stops_and_transfers_while_the_first_is_still_finishing_up() {
    let harness = Harness::start().await;
    std::fs::create_dir_all(harness.source.path().join("data")).unwrap();
    std::fs::write(harness.source.path().join("data/a.txt"), b"one").unwrap();
    std::fs::create_dir_all(harness.source.path().join("cache-data")).unwrap();
    std::fs::write(harness.source.path().join("cache-data/b.txt"), b"two").unwrap();

    begin_migration_for(&harness.node, instance(), volume()).await;
    begin_migration_for(&harness.node, instance2(), volume2()).await;

    let engine = DeltaEngine::default();
    let registry = harness.source_registry();
    let transfer_gate = Arc::new(tokio::sync::Semaphore::new(1));
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let handoff = Arc::new(tokio::sync::Notify::new());

    let control1 = GatedControl {
        log: log.clone(),
        label: "one",
        block_start_until: Some(handoff.clone()),
        signal_on_remove: None,
    };
    let control2 = GatedControl {
        log: log.clone(),
        label: "two",
        block_start_until: None,
        signal_on_remove: Some(handoff.clone()),
    };

    let job1 = MigrationJob {
        instance: instance(),
        source_node: "source".into(),
        target_node: "target".into(),
        target_addr: harness.target_addr,
        volumes: vec![volume()],
        registry: Some(&registry),
        engine: &engine,
        control: &control1,
        sync_progress: None,
        transfer_gate: transfer_gate.clone(),
    };
    let job2 = MigrationJob {
        instance: instance2(),
        source_node: "source".into(),
        target_node: "target".into(),
        target_addr: harness.target_addr,
        volumes: vec![volume2()],
        registry: Some(&registry),
        engine: &engine,
        control: &control2,
        sync_progress: None,
        transfer_gate,
    };

    let (result1, result2) = tokio::join!(run(&harness.node, &job1), run(&harness.node, &job2));
    result1.expect("job one completes once job two hands off");
    result2.expect("job two completes without waiting on job one's tail");

    let events = log.lock().unwrap().clone();
    let pos = |event: &str| events.iter().position(|e| e == event).unwrap_or_else(|| panic!("{event} missing from {events:?}"));

    // Job two's whole move, including its own start and cleanup, finished
    // before job one — still parked in `start_instance` — was allowed to log
    // its own `start`. That is only possible if job two's stop and transfer
    // never had to wait for job one's tail.
    assert!(pos("stop:two") < pos("start:one"), "events: {events:?}");
    assert!(pos("remove:two") < pos("start:one"), "events: {events:?}");

    let state = harness.node.state().await;
    assert_eq!(state.assignments[&instance()].node, "target");
    assert_eq!(state.assignments[&instance2()].node, "target");
}
