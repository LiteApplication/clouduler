//! Storage conformance. The openraft suite is the real check here; the tests
//! below cover what it does not, namely durability across a reopen.

#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use cl_raft::state::{Availability, NodeState, Request, Role};
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
        role: Role::Manager,
        rpc_addr: "10.10.0.1:7373".parse().expect("addr"),
        drives: BTreeMap::from([("fast".to_string(), PathBuf::from("/srv/fast"))]),
        availability: Availability::Active,
        labels: Default::default(),
            egress_networks: Default::default(),
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
