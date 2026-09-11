//! Storage conformance. The openraft suite is the real check here; the tests
//! below cover what it does not, namely durability across a reopen.

#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use cl_raft::ids::{InstanceId, VolumeId};
use cl_raft::state::{Availability, DesiredState, NodeState, Request, Role, VolumeState};
use cl_raft::store::{LogStore, StateMachineStore, Store};
use cl_raft::types::{StorageError, TypeConfig};
use openraft::storage::RaftStateMachine;
use openraft::testing::StoreBuilder;
use openraft::testing::Suite;
use tempfile::TempDir;

struct RedbBuilder;

impl StoreBuilder<TypeConfig, LogStore, StateMachineStore, TempDir> for RedbBuilder {
    async fn build(&self) -> Result<(TempDir, LogStore, StateMachineStore), StorageError> {
        // The temp dir is returned so it outlives the stores that point into it.
        let dir = TempDir::new().expect("temp dir");
        let store = Store::open(dir.path().join("raft.redb"))?;
        Ok((dir, store.log_store(), store.state_machine()))
    }
}

#[test]
fn openraft_storage_suite() -> Result<(), StorageError> {
    Suite::test_all(RedbBuilder)
}

fn node(name: &str) -> NodeState {
    NodeState {
        name: name.to_string(),
        id: uuid::Uuid::new_v4(),
        role: Role::Manager,
        rpc_addr: "10.10.0.1:7373".parse().expect("addr"),
        drives: BTreeMap::from([("fast".to_string(), PathBuf::from("/srv/fast"))]),
        availability: Availability::Active,
        labels: Default::default(),
        egress_networks: Default::default(),
        networks: Default::default(),
        open_ports: Default::default(),
        arch: Default::default(),
        binary_hash: Default::default(),
        node_pubkey: Default::default(),
        memory_total_bytes: Default::default(),
    }
}

/// Build one log entry carrying `request` at the given index.
fn entry(index: u64, request: Request) -> cl_raft::types::Entry {
    use openraft::{CommittedLeaderId, Entry, EntryPayload, LogId};
    Entry {
        log_id: LogId::new(CommittedLeaderId::new(1, 0), index),
        payload: EntryPayload::Normal(request),
    }
}

#[tokio::test]
async fn applied_state_survives_a_reopen() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("raft.redb");

    {
        let store = Store::open(&path).expect("open");
        let mut sm = store.state_machine();
        let responses = sm
            .apply([entry(1, Request::UpsertNode(node("n1")))])
            .await
            .expect("apply");
        assert!(responses[0].is_ok());
    }

    let store = Store::open(&path).expect("reopen");
    let mut sm = store.state_machine();
    let (last_applied, _) = sm.applied_state().await.expect("applied state");
    assert_eq!(last_applied.map(|id| id.index), Some(1));

    let shared = store.shared_state();
    let guard = shared.read().await;
    assert!(guard.data.nodes.contains_key("n1"));
    assert_eq!(guard.data.version, 1);
}

#[tokio::test]
async fn rejected_commands_still_advance_the_applied_id() {
    let dir = TempDir::new().expect("temp dir");
    let store = Store::open(dir.path().join("raft.redb")).expect("open");
    let mut sm = store.state_machine();

    // Setting availability on a node that does not exist is refused, but the
    // entry is still committed, so raft must see it as applied.
    let responses = sm
        .apply([entry(
            1,
            Request::SetAvailability {
                node: "ghost".into(),
                availability: Availability::Drain,
            },
        )])
        .await
        .expect("apply");
    assert!(!responses[0].is_ok());

    let (last_applied, _) = sm.applied_state().await.expect("applied state");
    assert_eq!(last_applied.map(|id| id.index), Some(1));
}

#[tokio::test]
async fn snapshots_round_trip_the_state_machine() {
    use openraft::RaftSnapshotBuilder;

    let dir = TempDir::new().expect("temp dir");
    let store = Store::open(dir.path().join("raft.redb")).expect("open");
    let mut sm = store.state_machine();
    sm.apply([entry(1, Request::UpsertNode(node("n1")))])
        .await
        .expect("apply");

    let mut builder = sm.get_snapshot_builder().await;
    let snapshot = builder.build_snapshot().await.expect("build snapshot");
    assert_eq!(snapshot.meta.last_log_id.map(|id| id.index), Some(1));

    // Install it into a fresh store and the node roster should come back.
    let other_dir = TempDir::new().expect("temp dir");
    let other = Store::open(other_dir.path().join("raft.redb")).expect("open");
    let mut other_sm = other.state_machine();
    other_sm
        .install_snapshot(&snapshot.meta, snapshot.snapshot)
        .await
        .expect("install");

    let shared = other.shared_state();
    let guard = shared.read().await;
    assert!(guard.data.nodes.contains_key("n1"));
}

/// `force_quorum` on its own must not touch any assignment's `desired` state
/// - that would be indistinguishable from an operator deliberately stopping
/// a stack, and there would be nothing left recording it should come back.
/// Only `emergency` (below) treats "not the recovering stack" specially, and
/// even that leaves `desired` alone in favour of `emergency_stack`.
#[tokio::test]
async fn force_quorum_leaves_assignments_desired_state_alone() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("raft.redb");

    let instance = InstanceId::new("site", "web", 0);
    {
        let store = Store::open(&path).expect("open");
        let mut sm = store.state_machine();
        let responses = sm
            .apply([
                entry(1, Request::UpsertNode(node("n1"))),
                entry(
                    2,
                    Request::Assign {
                        instance: instance.clone(),
                        node: "n1".to_string(),
                        drives: Default::default(),
                    },
                ),
            ])
            .await
            .expect("apply");
        assert!(responses[0].is_ok());
        assert!(responses[1].is_ok());
    }

    let store = Store::open(&path).expect("reopen");
    store
        .force_quorum(1, "10.10.0.2:7373".parse().expect("addr"))
        .await
        .expect("force quorum");

    let shared = store.shared_state();
    let guard = shared.read().await;
    let assignment = guard.data.assignments.get(&instance).expect("assignment");
    assert_eq!(assignment.desired, DesiredState::Running);
    assert_eq!(guard.data.emergency_stack, None);
}

#[tokio::test]
async fn emergency_sets_the_flag_marks_other_nodes_unreachable_and_stamps_volumes() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("raft.redb");

    let volume = VolumeId::new("fast", "data");
    {
        let store = Store::open(&path).expect("open");
        let mut sm = store.state_machine();
        let responses = sm
            .apply([
                entry(1, Request::UpsertNode(node("n1"))),
                entry(2, Request::UpsertNode(node("n2"))),
                entry(
                    3,
                    Request::UpsertVolume {
                        id: volume.clone(),
                        primary: "n2".to_string(),
                    },
                ),
            ])
            .await
            .expect("apply");
        for r in &responses {
            assert!(r.is_ok(), "{r:?}");
        }
    }

    let store = Store::open(&path).expect("reopen");
    let untouched = VolumeId::new("bulk", "other");
    store
        .emergency(
            1,
            "10.10.0.1:7373".parse().expect("addr"),
            "n1",
            "site",
            &[volume.clone()].into(),
        )
        .await
        .expect("emergency");

    let shared = store.shared_state();
    let guard = shared.read().await;
    assert_eq!(guard.data.emergency_stack.as_deref(), Some("site"));
    assert!(guard.data.unreachable.contains("n2"));
    assert!(!guard.data.unreachable.contains("n1"));

    let stamped = guard.data.volumes.get(&volume).expect("volume");
    assert_eq!(stamped.primary, "n1");
    assert_eq!(stamped.replicas.get("n1"), Some(&stamped.generation));
    assert!(!guard.data.volumes.contains_key(&untouched));
}

/// A volume `emergency` is told about that was never registered through raft
/// (a brand new one this node has local data for, but never wrote
/// `UpsertVolume` for before losing quorum) still needs to come out of this
/// as authoritative on this node, not silently skipped.
#[tokio::test]
async fn emergency_registers_a_volume_it_has_never_seen_before() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("raft.redb");
    {
        let store = Store::open(&path).expect("open");
        let mut sm = store.state_machine();
        sm.apply([entry(1, Request::UpsertNode(node("n1")))])
            .await
            .expect("apply");
    }

    let store = Store::open(&path).expect("reopen");
    let volume = VolumeId::new("fast", "fresh");
    store
        .emergency(
            1,
            "10.10.0.1:7373".parse().expect("addr"),
            "n1",
            "site",
            &[volume.clone()].into(),
        )
        .await
        .expect("emergency");

    let shared = store.shared_state();
    let guard = shared.read().await;
    let stamped: &VolumeState = guard.data.volumes.get(&volume).expect("volume");
    assert_eq!(stamped.primary, "n1");
    assert_eq!(stamped.generation, 1);
    assert_eq!(stamped.replicas.get("n1"), Some(&1));
}

#[tokio::test]
async fn restore_state_replaces_content_but_never_membership() {
    use cl_raft::state::{ClusterConfigEntry, ConfigEntry, SecretEntry, StackState};

    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("raft.redb");

    let instance = InstanceId::new("site", "web", 0);
    {
        let store = Store::open(&path).expect("open");
        let mut sm = store.state_machine();
        sm.apply([
            entry(1, Request::UpsertNode(node("n1"))),
            entry(
                2,
                Request::Assign {
                    instance: instance.clone(),
                    node: "n1".to_string(),
                    drives: Default::default(),
                },
            ),
        ])
        .await
        .expect("apply");
    }

    // Give this node's local log/membership a shape restore must leave alone.
    let store = Store::open(&path).expect("reopen");
    store
        .force_quorum(1, "10.10.0.1:7373".parse().expect("addr"))
        .await
        .expect("force quorum");

    let membership_before = {
        let shared = store.shared_state();
        let guard = shared.read().await;
        guard.membership.clone()
    };

    let mut stacks = BTreeMap::new();
    stacks.insert(
        "web".to_string(),
        StackState {
            name: "web".to_string(),
            spec: "services: {}\n".to_string(),
            revision: 1,
            configs: Default::default(),
            secrets: Default::default(),
            used_configs: Default::default(),
            used_secrets: Default::default(),
        },
    );
    let mut configs = BTreeMap::new();
    configs.insert(
        "nginx.conf".to_string(),
        ConfigEntry {
            name: "nginx.conf".to_string(),
            content: "c2VydmVyIHt9".to_string(),
            revision: 1,
            owner: None,
        },
    );
    let mut secrets = BTreeMap::new();
    secrets.insert(
        "db-password".to_string(),
        SecretEntry {
            name: "db-password".to_string(),
            sealed: "not-a-real-blob".to_string(),
            revision: 1,
            key_fingerprint: "deadbeef".to_string(),
            owner: None,
        },
    );
    let cluster_config = Some(ClusterConfigEntry {
        yaml: "nodes: []\n".to_string(),
        generation: 1,
    });

    store
        .restore_state(
            cluster_config.clone(),
            stacks.clone(),
            configs.clone(),
            secrets.clone(),
        )
        .await
        .expect("restore");

    let shared = store.shared_state();
    {
        let guard = shared.read().await;
        assert_eq!(guard.data.cluster_config, cluster_config);
        assert_eq!(guard.data.stacks, stacks);
        assert_eq!(guard.data.configs, configs);
        assert_eq!(guard.data.secrets, secrets);
        // Restore is content recovery, not membership recovery.
        assert_eq!(guard.membership, membership_before);
        // The pre-existing assignment (unrelated to the four restored
        // fields) is left untouched.
        assert!(guard.data.assignments.contains_key(&instance));
    }
    drop(shared);
    drop(store);

    // Persists across a reopen, like every other write to the state machine.
    let reopened = Store::open(&path).expect("reopen after restore");
    let shared = reopened.shared_state();
    let guard = shared.read().await;
    assert_eq!(guard.data.stacks, stacks);
    assert_eq!(guard.membership, membership_before);
}
