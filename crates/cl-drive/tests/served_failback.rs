//! A served drive whose preferred server comes back after a failover, over a
//! real single-node raft cluster (so `ServedAuthority` changes go through
//! `apply`) and a real loopback transport between two temp directories
//! standing in for the two candidates' copies.
//!
//! `n1` is first in `served_by` (the preferred server), `n2` the standby.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cl_config::Failback;
use cl_drive::served::failback::{plan_push, Direction, ServedHost, ServedSupervisor};
use cl_drive::{DeltaEngine, DriveRegistry, ProgressSink, SyncEngine};
use cl_raft::ids::node_id;
use cl_raft::node::{ClusterNode, Options};
use cl_raft::served::{ServedAuthority, ServedOp};
use cl_raft::state::{Availability, NodeState, Request, Role};
use cl_raft::{ClusterState, VolumeId};
use openraft::BasicNode;
use tempfile::TempDir;
use tokio::task::JoinHandle;

const DRIVE: &str = "fast";

fn keys() -> cl_raft::KeyWatch {
    static KEYS: std::sync::OnceLock<cl_raft::KeyWatch> = std::sync::OnceLock::new();
    KEYS.get_or_init(cl_raft::throwaway_key).clone()
}

fn free_addr() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn volume() -> VolumeId {
    VolumeId::new(DRIVE, "data")
}

fn served_by() -> BTreeMap<String, Vec<String>> {
    BTreeMap::from([(DRIVE.to_string(), vec!["n1".to_string(), "n2".to_string()])])
}

struct Candidate {
    registry: Arc<DriveRegistry>,
    addr: SocketAddr,
    listener: JoinHandle<()>,
    root: TempDir,
    _state: TempDir,
}

impl Candidate {
    async fn start(name: &str) -> Self {
        let root = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let registry = Arc::new(
            DriveRegistry::from_drives(
                name,
                BTreeMap::from([(DRIVE.to_string(), root.path().to_path_buf())]),
            )
            .with_served_by(served_by())
            .with_state_dir(state.path().to_path_buf()),
        );
        let addr = free_addr();
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let serving = registry.clone();
        let handle = tokio::spawn(async move {
            let _ = cl_drive::transport::serve_listener(listener, serving, keys()).await;
        });
        Self {
            registry,
            addr,
            listener: handle,
            root,
            _state: state,
        }
    }

    fn dir(&self) -> PathBuf {
        self.root.path().join("data")
    }

    fn write(&self, name: &str, content: &str) {
        std::fs::create_dir_all(self.dir()).unwrap();
        std::fs::write(self.dir().join(name), content).unwrap();
    }

    fn read(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.dir().join(name)).ok()
    }
}

#[derive(Default)]
struct TestHost {
    peers: BTreeMap<String, SocketAddr>,
    suspended: AtomicUsize,
    resumed: AtomicUsize,
}

impl ServedHost for TestHost {
    fn failback(&self, _drive: &str) -> Failback {
        Failback::Auto
    }
    fn suspend_export(&self, _drive: &str) {
        self.suspended.fetch_add(1, Ordering::SeqCst);
    }
    fn resume_export(&self, _drive: &str) {
        self.resumed.fetch_add(1, Ordering::SeqCst);
    }
    fn peer_addr(&self, node: &str, _state: &ClusterState) -> Option<SocketAddr> {
        self.peers.get(node).copied()
    }
}

struct World {
    _dir: TempDir,
    raft: Arc<ClusterNode>,
    n1: Candidate,
    n2: Candidate,
    host: Arc<TestHost>,
    engine: Arc<DeltaEngine>,
}

fn node_state(name: &str) -> NodeState {
    NodeState {
        name: name.into(),
        id: uuid::Uuid::new_v4(),
        role: Role::Manager,
        rpc_addr: free_addr(),
        drives: BTreeMap::from([(DRIVE.to_string(), PathBuf::from("/unused"))]),
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
        gpus: Default::default(),
    }
}

impl World {
    async fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let addr = free_addr();
        let id = uuid::Uuid::new_v4();
        let raft = ClusterNode::start(Options {
            name: "n2".into(),
            id,
            rpc_addr: addr,
            data_dir: dir.path().join("raft"),
            cluster_name: "test".into(),
            keys: cl_raft::throwaway_key(),
        })
        .await
        .unwrap();
        raft.initialize(BTreeMap::from([(
            node_id(id),
            BasicNode::new(addr.to_string()),
        )]))
        .await
        .unwrap();
        for _ in 0..100 {
            if raft.is_leader() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(raft.is_leader());
        let raft = Arc::new(raft);

        for name in ["n1", "n2"] {
            raft.submit(Request::UpsertNode(node_state(name)))
                .await
                .unwrap();
        }
        raft.submit(Request::UpsertVolume {
            id: volume(),
            primary: "n1".into(),
        })
        .await
        .unwrap();

        let n1 = Candidate::start("n1").await;
        let n2 = Candidate::start("n2").await;
        let host = Arc::new(TestHost {
            peers: BTreeMap::from([("n1".to_string(), n1.addr), ("n2".to_string(), n2.addr)]),
            ..Default::default()
        });
        Self {
            _dir: dir,
            raft,
            n1,
            n2,
            host,
            engine: Arc::new(DeltaEngine::new(keys())),
        }
    }

    async fn served(&self, op: ServedOp) {
        let response = self
            .raft
            .submit(Request::Served {
                drive: DRIVE.into(),
                op,
            })
            .await
            .unwrap();
        assert_eq!(response.error, None);
    }

    async fn reachable(&self, node: &str, reachable: bool) {
        self.raft
            .submit(Request::SetReachable {
                node: node.into(),
                reachable,
            })
            .await
            .unwrap();
    }

    async fn authority(&self) -> ServedAuthority {
        self.raft.state().await.served[DRIVE].clone()
    }

    /// One scheduled push from `from` to `to`: checked first, recorded on
    /// success, exactly as the sync scheduler does it.
    async fn scheduled_push(&self, from: &Candidate, to: &Candidate, to_name: &str) -> Direction {
        let authority = self.raft.state().await.served.get(DRIVE).cloned();
        let plan = plan_push(
            self.engine.as_ref(),
            &from.registry,
            &volume(),
            to.addr,
            to_name,
            authority.as_ref(),
        )
        .await
        .unwrap();
        if plan.direction == Direction::Push {
            self.engine
                .sync(
                    &from.dir(),
                    to.addr,
                    &volume(),
                    1,
                    None,
                    ProgressSink::none(),
                )
                .await
                .unwrap();
            from.registry.record_synced(&volume(), &plan.local.current);
        }
        plan.direction
    }

    fn supervisor(&self) -> Arc<ServedSupervisor<DeltaEngine>> {
        ServedSupervisor::new(
            self.raft.clone(),
            self.n2.registry.clone(),
            self.engine.clone(),
            self.host.clone(),
        )
        .with_drain(Duration::ZERO)
    }

    /// The history every scenario starts from: n1 serves and has pushed its
    /// data to n2, then n1 drops out and n2 takes over.
    async fn n1_failed_over_to_n2(&self) {
        self.n1.write("a.txt", "v1");
        self.served(ServedOp::Takeover {
            node: "n1".into(),
            served_by: vec!["n1".into(), "n2".into()],
        })
        .await;
        assert_eq!(
            self.scheduled_push(&self.n1, &self.n2, "n2").await,
            Direction::Push
        );
        self.reachable("n1", false).await;
        self.served(ServedOp::Takeover {
            node: "n2".into(),
            served_by: vec!["n1".into(), "n2".into()],
        })
        .await;
    }
}

fn exists(dir: &Path, name: &str) -> bool {
    dir.join(name).exists()
}

/// What the code did before the holder record existed: the first reachable
/// `served_by` candidate was the active server, so the returning n1 became
/// the source and pushed its copy over the standby's, wiping what had been
/// written there while it served.
#[tokio::test]
async fn without_a_holder_record_the_returning_server_pushes_its_stale_copy_over_the_standby() {
    let w = World::start().await;
    w.n1.write("a.txt", "v1");
    w.engine
        .sync(
            &w.n1.dir(),
            w.n2.addr,
            &volume(),
            1,
            None,
            ProgressSink::none(),
        )
        .await
        .unwrap();

    // n1 is down; n2 serves and is written.
    w.n2.write("a.txt", "v2, written on the standby");
    w.n2.write("b.txt", "new file on the standby");

    // n1 returns. With no record, it is the active server again...
    let no_record = ClusterState::default();
    assert_eq!(w.n1.registry.active_server(DRIVE, &no_record), Some("n1"));

    // ...and so the scheduler on n1 syncs n1 to n2 with nothing checking
    // which copy is newer.
    w.engine
        .sync(
            &w.n1.dir(),
            w.n2.addr,
            &volume(),
            1,
            None,
            ProgressSink::none(),
        )
        .await
        .unwrap();
    assert_eq!(w.n2.read("a.txt").as_deref(), Some("v1"));
    assert!(
        !exists(&w.n2.dir(), "b.txt"),
        "the standby's writes are gone"
    );
}

#[tokio::test]
async fn the_standby_keeps_serving_and_its_data_when_the_preferred_server_returns() {
    let w = World::start().await;
    w.n1_failed_over_to_n2().await;
    w.n2.write("a.txt", "v2, written on the standby");
    w.n2.write("b.txt", "new file on the standby");

    w.reachable("n1", true).await;
    let state = w.raft.state().await;
    assert_eq!(w.n1.registry.active_server(DRIVE, &state), Some("n2"));
    assert_eq!(w.n2.registry.active_server(DRIVE, &state), Some("n2"));

    // Were n1 to push, it would overwrite n2: the check refuses, because n1's
    // copy is untouched since they agreed while n2's was written.
    assert_eq!(
        w.scheduled_push(&w.n1, &w.n2, "n2").await,
        Direction::TargetAhead
    );
    assert_eq!(
        w.n2.read("a.txt").as_deref(),
        Some("v2, written on the standby")
    );
    assert!(exists(&w.n2.dir(), "b.txt"));
}

#[tokio::test]
async fn the_preferred_server_catches_up_from_the_standby_before_serving() {
    let w = World::start().await;
    w.n1_failed_over_to_n2().await;
    w.n2.write("a.txt", "v2, written on the standby");
    w.n2.write("b.txt", "new file on the standby");
    w.reachable("n1", true).await;

    let supervisor = w.supervisor();
    for _ in 0..40 {
        supervisor.pass().await;
        if w.authority().await.holder == "n1" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let authority = w.authority().await;
    assert_eq!(authority.holder, "n1");
    assert_eq!(authority.epoch, 3, "n1, then n2, then n1 again");
    assert!(authority.handover.is_none() && authority.conflict.is_none());
    // What was written on the standby is on the returning server now, and
    // still on the standby.
    for node in [&w.n1, &w.n2] {
        assert_eq!(
            node.read("a.txt").as_deref(),
            Some("v2, written on the standby")
        );
        assert_eq!(
            node.read("b.txt").as_deref(),
            Some("new file on the standby")
        );
    }
    assert_eq!(
        w.host.suspended.load(Ordering::SeqCst),
        1,
        "clients cut off once"
    );
    assert_eq!(
        w.host.resumed.load(Ordering::SeqCst),
        1,
        "and exported again"
    );
    let state = w.raft.state().await;
    assert_eq!(w.n1.registry.active_server(DRIVE, &state), Some("n1"));

    // n1 now serves and n2 is a clean standby again: pushing is allowed.
    assert_eq!(w.scheduled_push(&w.n1, &w.n2, "n2").await, Direction::Push);
}

async fn run_until<F: Fn(&ServedAuthority) -> bool>(w: &World, done: F) {
    let supervisor = w.supervisor();
    for _ in 0..40 {
        supervisor.pass().await;
        if done(&w.authority().await) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn a_preferred_server_written_just_before_it_dropped_out_gets_the_drive_back_intact() {
    let w = World::start().await;
    w.n1_failed_over_to_n2().await;
    // n1 took writes the standby never received; the standby was not written.
    w.n1.write("only-on-n1.txt", "written just before the crash");
    w.reachable("n1", true).await;

    assert_eq!(
        w.scheduled_push(&w.n2, &w.n1, "n1").await,
        Direction::TargetAhead,
        "n2 must not push over n1's newer data"
    );
    assert_eq!(
        w.n1.read("only-on-n1.txt").as_deref(),
        Some("written just before the crash")
    );

    run_until(&w, |a| a.holder == "n1").await;
    assert_eq!(w.authority().await.holder, "n1");
    assert_eq!(
        w.n1.read("only-on-n1.txt").as_deref(),
        Some("written just before the crash")
    );
    // The new holder's data now reaches the standby.
    assert_eq!(w.scheduled_push(&w.n1, &w.n2, "n2").await, Direction::Push);
    assert_eq!(
        w.n2.read("only-on-n1.txt").as_deref(),
        Some("written just before the crash")
    );
}

#[tokio::test]
async fn copies_written_on_both_sides_are_kept_and_reported_as_a_conflict() {
    let w = World::start().await;
    w.n1_failed_over_to_n2().await;
    // A partition: n1 kept taking writes locally while n2 served and was
    // written too.
    w.n1.write("only-on-n1.txt", "written on n1");
    w.n2.write("only-on-n2.txt", "written on n2");
    w.reachable("n1", true).await;

    run_until(&w, |a| a.conflict.is_some()).await;

    let authority = w.authority().await;
    let conflict = authority.conflict.expect("a conflict is recorded");
    assert_eq!(
        (conflict.holder.as_str(), conflict.other.as_str()),
        ("n2", "n1")
    );
    assert!(
        conflict.reason.contains("both copies"),
        "{}",
        conflict.reason
    );
    assert_eq!(
        authority.holder, "n2",
        "the drive keeps being served from n2"
    );
    assert!(authority.handover.is_none());
    assert_eq!(
        w.host.suspended.load(Ordering::SeqCst),
        0,
        "clients were never cut off"
    );
    // Nothing was touched on either side.
    assert!(exists(&w.n1.dir(), "only-on-n1.txt") && !exists(&w.n1.dir(), "only-on-n2.txt"));
    assert!(exists(&w.n2.dir(), "only-on-n2.txt") && !exists(&w.n2.dir(), "only-on-n1.txt"));

    // More passes change nothing.
    run_until(&w, |_| false).await;
    assert_eq!(w.host.suspended.load(Ordering::SeqCst), 0);
    assert!(!exists(&w.n1.dir(), "only-on-n2.txt"));

    // Resolving in favour of n2 lets n2's copy replace n1's, once.
    w.served(ServedOp::Resolve { keep: "n2".into() }).await;
    assert_eq!(w.scheduled_push(&w.n2, &w.n1, "n1").await, Direction::Push);
    assert!(exists(&w.n1.dir(), "only-on-n2.txt"));
    assert!(!exists(&w.n1.dir(), "only-on-n1.txt"));
}

#[tokio::test]
async fn a_final_sync_that_fails_leaves_the_standby_serving_and_is_not_retried() {
    let w = World::start().await;
    w.n1_failed_over_to_n2().await;
    w.n2.write("b.txt", "new file on the standby");
    w.reachable("n1", true).await;

    // The handover has begun, then n1 stops answering on the drive transport.
    w.served(ServedOp::BeginHandover {
        to: "n1".into(),
        at: 1,
    })
    .await;
    w.n1.listener.abort();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let supervisor = w.supervisor();
    for _ in 0..20 {
        supervisor.pass().await;
        if w.authority().await.blocked.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let authority = w.authority().await;
    assert_eq!(authority.holder, "n2", "the standby keeps serving");
    assert!(authority.handover.is_none());
    let block = authority.blocked.expect("the reason is recorded");
    assert_eq!(block.node, "n1");
    assert!(block.reason.contains("n1"), "{}", block.reason);
    assert_eq!(w.host.suspended.load(Ordering::SeqCst), 1);
    assert_eq!(
        w.host.resumed.load(Ordering::SeqCst),
        1,
        "exports come back"
    );
    assert!(exists(&w.n2.dir(), "b.txt"));

    // No retry storm: passes after the failure start nothing new.
    for _ in 0..10 {
        supervisor.pass().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(w.host.suspended.load(Ordering::SeqCst), 1);
    assert!(w.authority().await.handover.is_none());
}

#[tokio::test]
async fn an_unreachable_preferred_server_is_not_probed_and_nothing_starts() {
    let w = World::start().await;
    w.n1_failed_over_to_n2().await;
    let supervisor = w.supervisor();
    for _ in 0..5 {
        supervisor.pass().await;
    }
    assert_eq!(w.authority().await.holder, "n2");
    assert_eq!(w.host.suspended.load(Ordering::SeqCst), 0);
}
