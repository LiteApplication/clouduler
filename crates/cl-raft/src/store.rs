//! Persistent raft storage on redb.
//!
//! The log lives in its own table keyed by index. The state machine is written
//! out in full on every apply, which keeps recovery trivial: no log replay is
//! needed past the last applied id, and snapshots become a read rather than a
//! rebuild. The whole cluster state is a few kilobytes at this scale, so the
//! cost of rewriting it per command is not worth optimising away.

// openraft's StorageError is 224 bytes and is fixed by its trait signatures.
#![allow(clippy::result_large_err)]

use std::fmt::Debug;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::path::Path;
use std::sync::Arc;

use openraft::storage::{LogFlushed, RaftLogStorage, RaftStateMachine};
use openraft::{
    AnyError, EntryPayload, LogState, RaftLogReader, RaftSnapshotBuilder, StorageIOError,
};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::ids::NodeId;
use crate::state::{ClusterState, Response};
use crate::types::{
    Entry, LogId, Snapshot, SnapshotMeta, StorageError, StoredMembership, TypeConfig, Vote,
};

const LOGS: TableDefinition<u64, &[u8]> = TableDefinition::new("raft_logs");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("raft_meta");

const KEY_VOTE: &str = "vote";
const KEY_COMMITTED: &str = "committed";
const KEY_PURGED: &str = "purged";
const KEY_STATE_MACHINE: &str = "state_machine";
const KEY_SNAPSHOT: &str = "snapshot";

/// The applied state, persisted as a unit so it can never be half written.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StateMachine {
    pub last_applied: Option<LogId>,
    pub membership: StoredMembership,
    pub data: ClusterState,
}

/// A stored snapshot: its metadata plus the serialized state machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSnapshot {
    meta: SnapshotMeta,
    data: Vec<u8>,
}

/// Opens the redb file and hands out the two halves raft needs.
pub struct Store {
    db: Arc<Database>,
    sm: Arc<RwLock<StateMachine>>,
}

impl Store {
    /// Open or create the store at `path`, restoring any state machine already there.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let db = Database::create(path.as_ref()).map_err(read_err)?;

        // Creating the tables up front means every later read can assume they exist.
        let txn = db.begin_write().map_err(write_err)?;
        {
            txn.open_table(LOGS).map_err(write_err)?;
            txn.open_table(META).map_err(write_err)?;
        }
        txn.commit().map_err(write_err)?;

        let db = Arc::new(db);
        let sm = match read_meta(&db, KEY_STATE_MACHINE)? {
            Some(raw) => decode(&raw)?,
            None => StateMachine::default(),
        };

        Ok(Self {
            db,
            sm: Arc::new(RwLock::new(sm)),
        })
    }

    pub fn log_store(&self) -> LogStore {
        LogStore {
            db: self.db.clone(),
        }
    }

    pub fn state_machine(&self) -> StateMachineStore {
        StateMachineStore {
            db: self.db.clone(),
            sm: self.sm.clone(),
        }
    }

    /// Shared handle onto the applied state, for read only callers outside raft.
    pub fn shared_state(&self) -> Arc<RwLock<StateMachine>> {
        self.sm.clone()
    }

    /// Force this node to be the cluster's sole voter, bypassing consensus
    /// entirely by rewriting the local log and state machine directly.
    ///
    /// For a cluster that has lost quorum for good - a voter's local state
    /// gone forever, with no backup - normal `change_membership` can never
    /// run again, since driving it through raft itself needs the quorum
    /// that is missing. This is the same escape hatch etcd's
    /// `--force-new-cluster` or Consul's `peers.json` recovery provide: it
    /// appends one synthetic, already-committed membership entry and moves
    /// on, with no other voter ever asked to agree.
    ///
    /// Every node this store already knew about, including the lost voter,
    /// stays in the new membership's node map - only the voter set shrinks
    /// to `id` alone. That is deliberate: a worker resumes replicating
    /// without needing to be re-added as a learner, and the lost voter is
    /// demoted to a learner rather than dropped, so it can rejoin cleanly
    /// once it comes back.
    ///
    /// Must only run while the daemon owning this data directory is
    /// stopped: this writes straight past the raft runtime, so anything
    /// else with the file open would either fail to open it or race this
    /// write.
    pub async fn force_quorum(
        &self,
        id: NodeId,
        addr: std::net::SocketAddr,
    ) -> Result<openraft::Membership<NodeId, crate::types::Node>, StorageError> {
        let mut sm = self.sm.write().await;
        let membership = self.rewrite_membership(&mut sm, id, addr).await?;
        write_meta(&self.db, KEY_STATE_MACHINE, &*sm)?;
        Ok(membership)
    }

    /// `force_quorum`, plus the extra state `clouduler emergency` needs: only
    /// `stack`'s instances are allowed to start once this node comes back up
    /// (`ClusterState::emergency_stack`, cleared automatically once another
    /// voter is reachable again - see `Request::ClearEmergency`), every other
    /// currently known node is marked unreachable (so a served drive `stack`
    /// uses picks this node as its active server, the same
    /// first-reachable-candidate logic `drives.rs` always uses - see
    /// `docs/clouduler-emergency.md`), and every volume `stack` uses is
    /// stamped as current on this node, so nothing waits for, or tries to
    /// pull from, a copy elsewhere that may not even exist any more.
    ///
    /// `stack_volumes` is computed by the caller (`clouduler::node::emergency`)
    /// from the stack's own spec before this runs, so this crate does not have
    /// to depend on stack-file parsing here.
    pub async fn emergency(
        &self,
        id: NodeId,
        addr: std::net::SocketAddr,
        this_name: &str,
        stack: &str,
        stack_volumes: &std::collections::BTreeSet<crate::ids::VolumeId>,
    ) -> Result<openraft::Membership<NodeId, crate::types::Node>, StorageError> {
        let mut sm = self.sm.write().await;
        let membership = self.rewrite_membership(&mut sm, id, addr).await?;

        sm.data.emergency_stack = Some(stack.to_string());

        // The other managers' fate is unknown - some worker may still be
        // running an instance this node's stale copy of the state also
        // thinks is unassigned, or a redeploy that never reached quorum may
        // resurrect a container that conflicts with one still alive
        // elsewhere. Marking them unreachable rather than touching any
        // assignment's `desired` means nothing needs restoring by hand once
        // `emergency_stack` clears: every stack's own desired state was never
        // altered.
        let others: Vec<String> = sm.data.nodes.keys().filter(|n| n.as_str() != this_name).cloned().collect();
        sm.data.unreachable.extend(others);

        for volume in stack_volumes {
            match sm.data.volumes.get_mut(volume) {
                Some(existing) => {
                    existing.primary = this_name.to_string();
                    existing.replicas.insert(this_name.to_string(), existing.generation);
                }
                None => {
                    sm.data.volumes.insert(
                        volume.clone(),
                        crate::state::VolumeState {
                            id: volume.clone(),
                            generation: 1,
                            primary: this_name.to_string(),
                            replicas: std::collections::BTreeMap::from([(this_name.to_string(), 1)]),
                        },
                    );
                }
            }
        }

        write_meta(&self.db, KEY_STATE_MACHINE, &*sm)?;
        Ok(membership)
    }

    /// The shared part of `force_quorum`/`emergency`: append one synthetic,
    /// already-committed membership entry making `id` the cluster's sole
    /// voter, bypassing consensus entirely by rewriting the local log and
    /// state machine directly.
    ///
    /// For a cluster that has lost quorum for good - a voter's local state
    /// gone forever, with no backup - normal `change_membership` can never
    /// run again, since driving it through raft itself needs the quorum
    /// that is missing. This is the same escape hatch etcd's
    /// `--force-new-cluster` or Consul's `peers.json` recovery provide.
    ///
    /// Every node this store already knew about, including the lost voter,
    /// stays in the new membership's node map - only the voter set shrinks
    /// to `id` alone. That is deliberate: a worker resumes replicating
    /// without needing to be re-added as a learner, and the lost voter is
    /// demoted to a learner rather than dropped, so it can rejoin cleanly
    /// once it comes back.
    ///
    /// Leaves `sm`'s membership/log fields updated but does not persist: the
    /// caller writes the state machine once, after making whatever further
    /// changes it needs.
    async fn rewrite_membership(
        &self,
        sm: &mut StateMachine,
        id: NodeId,
        addr: std::net::SocketAddr,
    ) -> Result<openraft::Membership<NodeId, crate::types::Node>, StorageError> {
        let last_index = {
            let txn = self.db.begin_read().map_err(read_err)?;
            let table = txn.open_table(LOGS).map_err(read_err)?;
            let last = table.last().map_err(read_err)?;
            match last {
                Some((_, value)) => decode::<Entry>(value.value())?.log_id.index,
                None => 0,
            }
        };

        let current_term = read_meta(&self.db, KEY_VOTE)?
            .map(|raw| decode::<crate::types::Vote>(&raw))
            .transpose()?
            .map(|vote| vote.leader_id.get_term())
            .unwrap_or(0);
        let term = current_term + 1;

        let mut nodes: std::collections::BTreeMap<NodeId, crate::types::Node> = sm
            .membership
            .membership()
            .nodes()
            .map(|(node_id, node)| (*node_id, node.clone()))
            .collect();
        nodes.insert(id, crate::types::Node::new(addr.to_string()));

        let membership = openraft::Membership::new(vec![std::iter::once(id).collect()], nodes);
        let log_id = LogId::new(openraft::CommittedLeaderId::new(term, id), last_index + 1);

        let entry = Entry {
            log_id,
            payload: EntryPayload::Membership(membership.clone()),
        };
        {
            let txn = self.db.begin_write().map_err(write_err)?;
            {
                let mut table = txn.open_table(LOGS).map_err(write_err)?;
                let encoded = encode(&entry)?;
                table
                    .insert(entry.log_id.index, encoded.as_slice())
                    .map_err(write_err)?;
            }
            txn.commit().map_err(write_err)?;
        }

        write_meta(&self.db, KEY_COMMITTED, &Some(log_id))?;
        write_meta(&self.db, KEY_VOTE, &crate::types::Vote::new_committed(term, id))?;

        sm.last_applied = Some(log_id);
        sm.membership = StoredMembership::new(Some(log_id), membership.clone());

        Ok(membership)
    }
}

// -- log --------------------------------------------------------------------

#[derive(Clone)]
pub struct LogStore {
    db: Arc<Database>,
}

impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry>, StorageError> {
        // `RB` isn't `'static` (the trait doesn't require it), so it can't be
        // moved into `spawn_blocking` as-is; owned bounds can.
        let range = (range.start_bound().cloned(), range.end_bound().cloned());
        let db = self.db.clone();
        blocking(move || {
            let txn = db.begin_read().map_err(read_err)?;
            let table = txn.open_table(LOGS).map_err(read_err)?;
            let mut out = Vec::new();
            for row in table.range(range).map_err(read_err)? {
                let (_, value) = row.map_err(read_err)?;
                out.push(decode(value.value())?);
            }
            Ok(out)
        })
        .await
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError> {
        let db = self.db.clone();
        blocking(move || {
            let purged: Option<LogId> = read_meta(&db, KEY_PURGED)?.map(|raw| decode(&raw)).transpose()?;

            let txn = db.begin_read().map_err(read_err)?;
            let table = txn.open_table(LOGS).map_err(read_err)?;
            let last = match table.last().map_err(read_err)? {
                Some((_, value)) => {
                    let entry: Entry = decode(value.value())?;
                    Some(entry.log_id)
                }
                // With no entries left, the last purged id is the last id we know of.
                None => purged,
            };

            Ok(LogState { last_purged_log_id: purged, last_log_id: last })
        })
        .await
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote) -> Result<(), StorageError> {
        let db = self.db.clone();
        let vote = *vote;
        blocking(move || write_meta(&db, KEY_VOTE, &vote)).await
    }

    async fn read_vote(&mut self) -> Result<Option<Vote>, StorageError> {
        let db = self.db.clone();
        blocking(move || read_meta(&db, KEY_VOTE)?.map(|raw| decode(&raw)).transpose()).await
    }

    async fn save_committed(&mut self, committed: Option<LogId>) -> Result<(), StorageError> {
        let db = self.db.clone();
        blocking(move || write_meta(&db, KEY_COMMITTED, &committed)).await
    }

    async fn read_committed(&mut self) -> Result<Option<LogId>, StorageError> {
        let db = self.db.clone();
        blocking(move || match read_meta(&db, KEY_COMMITTED)? {
            Some(raw) => decode(&raw),
            None => Ok(None),
        })
        .await
    }

    async fn append<I>(&mut self, entries: I, callback: LogFlushed<TypeConfig>) -> Result<(), StorageError>
    where
        I: IntoIterator<Item = Entry> + Send,
        I::IntoIter: Send,
    {
        let db = self.db.clone();
        let entries: Vec<Entry> = entries.into_iter().collect();
        blocking(move || {
            let txn = db.begin_write().map_err(write_err)?;
            {
                let mut table = txn.open_table(LOGS).map_err(write_err)?;
                for entry in entries {
                    let encoded = encode(&entry)?;
                    table
                        .insert(entry.log_id.index, encoded.as_slice())
                        .map_err(write_err)?;
                }
            }
            // redb commits durably, so the callback can fire as soon as this returns.
            txn.commit().map_err(write_err)
        })
        .await?;
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId) -> Result<(), StorageError> {
        let db = self.db.clone();
        blocking(move || {
            let txn = db.begin_write().map_err(write_err)?;
            {
                let mut table = txn.open_table(LOGS).map_err(write_err)?;
                table.retain_in(log_id.index.., |_, _| false).map_err(write_err)?;
            }
            txn.commit().map_err(write_err)
        })
        .await
    }

    async fn purge(&mut self, log_id: LogId) -> Result<(), StorageError> {
        let db = self.db.clone();
        blocking(move || {
            let txn = db.begin_write().map_err(write_err)?;
            {
                let mut table = txn.open_table(LOGS).map_err(write_err)?;
                table
                    .retain_in(..=log_id.index, |_, _| false)
                    .map_err(write_err)?;
                let mut meta = txn.open_table(META).map_err(write_err)?;
                let encoded = encode(&log_id)?;
                meta.insert(KEY_PURGED, encoded.as_slice()).map_err(write_err)?;
            }
            txn.commit().map_err(write_err)
        })
        .await
    }
}

// -- state machine ----------------------------------------------------------

#[derive(Clone)]
pub struct StateMachineStore {
    db: Arc<Database>,
    sm: Arc<RwLock<StateMachine>>,
}

impl StateMachineStore {
    /// Persist the in memory state machine. Called while the write lock is held.
    async fn persist(&self, sm: &StateMachine) -> Result<(), StorageError> {
        let db = self.db.clone();
        let sm = sm.clone();
        blocking(move || write_meta(&db, KEY_STATE_MACHINE, &sm)).await
    }
}

impl RaftStateMachine<TypeConfig> for StateMachineStore {
    type SnapshotBuilder = Self;

    async fn applied_state(&mut self) -> Result<(Option<LogId>, StoredMembership), StorageError> {
        let sm = self.sm.read().await;
        Ok((sm.last_applied, sm.membership.clone()))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<Response>, StorageError>
    where
        I: IntoIterator<Item = Entry> + Send,
        I::IntoIter: Send,
    {
        let mut sm = self.sm.write().await;
        let mut responses = Vec::new();

        for entry in entries {
            sm.last_applied = Some(entry.log_id);
            let response = match entry.payload {
                // Blank entries mark a new term, nothing to apply.
                EntryPayload::Blank => Response {
                    version: sm.data.version,
                    error: None,
                },
                EntryPayload::Normal(ref request) => sm.data.apply(request),
                EntryPayload::Membership(ref membership) => {
                    sm.membership = StoredMembership::new(Some(entry.log_id), membership.clone());
                    Response {
                        version: sm.data.version,
                        error: None,
                    }
                }
            };
            responses.push(response);
        }

        self.persist(&sm).await?;
        Ok(responses)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(&mut self) -> Result<Box<Cursor<Vec<u8>>>, StorageError> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError> {
        let data = snapshot.into_inner();
        let restored: StateMachine = decode(&data)?;

        let mut sm = self.sm.write().await;
        *sm = restored;
        self.persist(&sm).await?;

        let db = self.db.clone();
        let stored = StoredSnapshot { meta: meta.clone(), data };
        blocking(move || write_meta(&db, KEY_SNAPSHOT, &stored)).await
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot>, StorageError> {
        let db = self.db.clone();
        blocking(move || {
            let Some(raw) = read_meta(&db, KEY_SNAPSHOT)? else {
                return Ok(None);
            };
            let stored: StoredSnapshot = decode(&raw)?;
            Ok(Some(Snapshot {
                meta: stored.meta,
                snapshot: Box::new(Cursor::new(stored.data)),
            }))
        })
        .await
    }
}

impl RaftSnapshotBuilder<TypeConfig> for StateMachineStore {
    async fn build_snapshot(&mut self) -> Result<Snapshot, StorageError> {
        let (data, meta) = {
            let sm = self.sm.read().await;
            let data = encode(&*sm)?;
            let meta = SnapshotMeta {
                last_log_id: sm.last_applied,
                last_membership: sm.membership.clone(),
                // Version makes successive snapshots distinguishable, which is
                // all openraft needs from the id.
                snapshot_id: format!(
                    "{}-{}",
                    sm.last_applied.map(|id| id.index).unwrap_or(0),
                    sm.data.version
                ),
            };
            (data, meta)
        };

        let db = self.db.clone();
        let stored = StoredSnapshot { meta: meta.clone(), data: data.clone() };
        blocking(move || write_meta(&db, KEY_SNAPSHOT, &stored)).await?;

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

// -- helpers ----------------------------------------------------------------

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, StorageError> {
    serde_json::to_vec(value).map_err(|e| StorageIOError::<NodeId>::write(AnyError::new(&e)).into())
}

fn decode<T: for<'de> Deserialize<'de>>(raw: &[u8]) -> Result<T, StorageError> {
    serde_json::from_slice(raw).map_err(|e| StorageIOError::<NodeId>::read(AnyError::new(&e)).into())
}

fn read_meta(db: &Database, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
    let txn = db.begin_read().map_err(read_err)?;
    let table = txn.open_table(META).map_err(read_err)?;
    Ok(table
        .get(key)
        .map_err(read_err)?
        .map(|value| value.value().to_vec()))
}

fn write_meta<T: Serialize>(db: &Database, key: &str, value: &T) -> Result<(), StorageError> {
    let encoded = encode(value)?;
    let txn = db.begin_write().map_err(write_err)?;
    {
        let mut table = txn.open_table(META).map_err(write_err)?;
        table.insert(key, encoded.as_slice()).map_err(write_err)?;
    }
    txn.commit().map_err(write_err)
}

fn read_err<E: std::error::Error + 'static>(e: E) -> StorageError {
    StorageIOError::<NodeId>::read(AnyError::new(&e)).into()
}

fn write_err<E: std::error::Error + 'static>(e: E) -> StorageError {
    StorageIOError::<NodeId>::write(AnyError::new(&e)).into()
}

/// Runs a redb operation on the blocking thread pool.
///
/// redb's `commit()` does a real `fsync`, which on the aarch64 nodes' SD/USB
/// storage routinely takes well past the raft heartbeat's soft ttl (see
/// `cl-raft::network::Channel::call`) whenever something else on the box is
/// also hitting disk. Called inline on the async task handling that peer's
/// connection, a slow commit stalls the response to a totally unrelated
/// heartbeat or vote RPC on the same executor, which is what actually shows
/// up as a raft "Unreachable" timeout — the peer was never actually
/// unreachable, this node just hadn't gotten back to answering it yet.
/// Running it here instead lets the executor keep serving other RPCs while
/// the disk catches up.
async fn blocking<T, F>(f: F) -> Result<T, StorageError>
where
    F: FnOnce() -> Result<T, StorageError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(write_err)?
}
