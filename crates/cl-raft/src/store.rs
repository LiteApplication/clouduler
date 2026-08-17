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
        let txn = self.db.begin_read().map_err(read_err)?;
        let table = txn.open_table(LOGS).map_err(read_err)?;
        let mut out = Vec::new();
        for row in table.range(range).map_err(read_err)? {
            let (_, value) = row.map_err(read_err)?;
            out.push(decode(value.value())?);
        }
        Ok(out)
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError> {
        let purged: Option<LogId> = read_meta(&self.db, KEY_PURGED)?
            .map(|raw| decode(&raw))
            .transpose()?;

        let txn = self.db.begin_read().map_err(read_err)?;
        let table = txn.open_table(LOGS).map_err(read_err)?;
        let last = match table.last().map_err(read_err)? {
            Some((_, value)) => {
                let entry: Entry = decode(value.value())?;
                Some(entry.log_id)
            }
            // With no entries left, the last purged id is the last id we know of.
            None => purged,
        };

        Ok(LogState {
            last_purged_log_id: purged,
            last_log_id: last,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote) -> Result<(), StorageError> {
        write_meta(&self.db, KEY_VOTE, vote)
    }

    async fn read_vote(&mut self) -> Result<Option<Vote>, StorageError> {
        read_meta(&self.db, KEY_VOTE)?.map(|raw| decode(&raw)).transpose()
    }

    async fn save_committed(&mut self, committed: Option<LogId>) -> Result<(), StorageError> {
        write_meta(&self.db, KEY_COMMITTED, &committed)
    }

    async fn read_committed(&mut self) -> Result<Option<LogId>, StorageError> {
        match read_meta(&self.db, KEY_COMMITTED)? {
            Some(raw) => decode(&raw),
            None => Ok(None),
        }
    }

    async fn append<I>(&mut self, entries: I, callback: LogFlushed<TypeConfig>) -> Result<(), StorageError>
    where
        I: IntoIterator<Item = Entry> + Send,
        I::IntoIter: Send,
    {
        let txn = self.db.begin_write().map_err(write_err)?;
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
        txn.commit().map_err(write_err)?;
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId) -> Result<(), StorageError> {
        let txn = self.db.begin_write().map_err(write_err)?;
        {
            let mut table = txn.open_table(LOGS).map_err(write_err)?;
            table.retain_in(log_id.index.., |_, _| false).map_err(write_err)?;
        }
        txn.commit().map_err(write_err)
    }

    async fn purge(&mut self, log_id: LogId) -> Result<(), StorageError> {
        let txn = self.db.begin_write().map_err(write_err)?;
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
    fn persist(&self, sm: &StateMachine) -> Result<(), StorageError> {
        write_meta(&self.db, KEY_STATE_MACHINE, sm)
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

        self.persist(&sm)?;
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
        self.persist(&sm)?;
        write_meta(
            &self.db,
            KEY_SNAPSHOT,
            &StoredSnapshot {
                meta: meta.clone(),
                data,
            },
        )
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot>, StorageError> {
        let Some(raw) = read_meta(&self.db, KEY_SNAPSHOT)? else {
            return Ok(None);
        };
        let stored: StoredSnapshot = decode(&raw)?;
        Ok(Some(Snapshot {
            meta: stored.meta,
            snapshot: Box::new(Cursor::new(stored.data)),
        }))
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

        write_meta(
            &self.db,
            KEY_SNAPSHOT,
            &StoredSnapshot {
                meta: meta.clone(),
                data: data.clone(),
            },
        )?;

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
