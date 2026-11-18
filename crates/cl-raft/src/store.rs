//! Persistent raft storage on redb. The state machine is written out in full on
//! every apply (it is a few kilobytes), so recovery needs no log replay past the
//! last applied id and snapshots are a read.

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateMachine {
    pub last_applied: Option<LogId>,
    pub membership: StoredMembership,
    pub data: ClusterState,
    /// Schema this blob was written at (see `crate::migrations`); absent reads as `0`.
    #[serde(default)]
    pub schema_version: u32,
}

impl Default for StateMachine {
    /// A fresh node is already in the current shape: stamp the current version, not
    /// `0`, or the first real migration would re-run on data it was not meant for.
    fn default() -> Self {
        Self {
            last_applied: None,
            membership: StoredMembership::default(),
            data: ClusterState::default(),
            schema_version: crate::state::CURRENT_SCHEMA_VERSION,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSnapshot {
    meta: SnapshotMeta,
    data: Vec<u8>,
}

pub struct Store {
    db: Arc<Database>,
    sm: Arc<RwLock<StateMachine>>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let db = Database::create(path.as_ref()).map_err(read_err)?;

        let txn = db.begin_write().map_err(write_err)?;
        {
            txn.open_table(LOGS).map_err(write_err)?;
            txn.open_table(META).map_err(write_err)?;
        }
        txn.commit().map_err(write_err)?;

        let db = Arc::new(db);
        let sm = match read_meta(&db, KEY_STATE_MACHINE)? {
            Some(raw) => match load_state_machine(&raw)? {
                LoadedStateMachine::Current(sm) => *sm,
                // This node's own local data has no fallback: a hard, visible failure beats
                // silently starting on data this build does not understand (unlike
                // `install_snapshot`, which has a live cluster to fall back on).
                LoadedStateMachine::TooNew { found } => {
                    return Err(StorageIOError::<NodeId>::read(AnyError::error(format!(
                        "local state machine is schema_version {found}, newer than this build's {} - this looks like a downgrade after a newer version already ran here; upgrade this node rather than downgrading it",
                        crate::state::CURRENT_SCHEMA_VERSION
                    )))
                    .into());
                }
            },
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

    pub fn shared_state(&self) -> Arc<RwLock<StateMachine>> {
        self.sm.clone()
    }

    /// Force this node to be the cluster's sole voter by rewriting the local log
    /// and state machine directly, for a cluster that lost quorum for good (like
    /// etcd's `--force-new-cluster`). Appends one synthetic committed membership
    /// entry; other known nodes stay in the node map (the lost voter becomes a
    /// learner, so it can rejoin and workers keep replicating).
    ///
    /// Must only run while the daemon owning this data directory is stopped.
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

    /// `force_quorum` plus what `clouduler emergency` needs: only `stack`'s instances
    /// may start once this node is back (`ClusterState::emergency_stacks`, cleared
    /// once another voter is reachable, see `Request::ClearEmergency`), every other
    /// known node is marked unreachable (so a served drive `stack` uses is served
    /// here, see `docs/clouduler-emergency.md`), and every volume `stack` uses is
    /// stamped current on this node. `stack_volumes` is computed by the caller so
    /// this crate need not parse stack files.
    pub async fn emergency(
        &self,
        id: NodeId,
        addr: std::net::SocketAddr,
        this_name: &str,
        stacks: &std::collections::BTreeSet<String>,
        stack_volumes: &std::collections::BTreeSet<crate::ids::VolumeId>,
    ) -> Result<openraft::Membership<NodeId, crate::types::Node>, StorageError> {
        let mut sm = self.sm.write().await;
        let membership = self.rewrite_membership(&mut sm, id, addr).await?;

        sm.data.emergency_stacks = stacks.clone();

        // The other managers' fate is unknown: a worker may still run an instance this
        // stale state thinks is unassigned. Marking them unreachable rather than
        // touching any `desired` leaves nothing to restore once `emergency_stacks` clears.
        let others: Vec<String> = sm
            .data
            .nodes
            .keys()
            .filter(|n| n.as_str() != this_name)
            .cloned()
            .collect();
        sm.data.unreachable.extend(others);

        for volume in stack_volumes {
            match sm.data.volumes.get_mut(volume) {
                Some(existing) => {
                    existing.primary = this_name.to_string();
                    existing
                        .replicas
                        .insert(this_name.to_string(), existing.generation);
                }
                None => {
                    sm.data.volumes.insert(
                        volume.clone(),
                        crate::state::VolumeState {
                            id: volume.clone(),
                            generation: 1,
                            primary: this_name.to_string(),
                            replicas: std::collections::BTreeMap::from([(
                                this_name.to_string(),
                                1,
                            )]),
                            frozen: None,
                            sync_refused: None,
                            looks_empty: false,
                        },
                    );
                }
            }
        }

        write_meta(&self.db, KEY_STATE_MACHINE, &*sm)?;
        Ok(membership)
    }

    /// Replaces `cluster_config`/`stacks`/`configs`/`secrets` with content from a
    /// git backup (see `cl-backup`), for `clouduler restore`. Never touches
    /// membership or the log: a backup restores content, not quorum (run
    /// `force_quorum`/`emergency` first for disaster recovery).
    /// `assignments`/`volumes`/`certificates`/`dns_records`/`unreachable`/
    /// `emergency_stacks` are left alone: the scheduler reconciles against the bumped
    /// `StackState.revision`, and touching them risks orphaning containers.
    ///
    /// Must only run while the daemon is stopped.
    pub async fn restore_state(
        &self,
        cluster_config: Option<crate::state::ClusterConfigEntry>,
        stacks: std::collections::BTreeMap<String, crate::state::StackState>,
        configs: std::collections::BTreeMap<String, crate::state::ConfigEntry>,
        secrets: std::collections::BTreeMap<String, crate::state::SecretEntry>,
    ) -> Result<(), StorageError> {
        let mut sm = self.sm.write().await;
        sm.data.cluster_config = cluster_config;
        sm.data.stacks = stacks;
        sm.data.configs = configs;
        sm.data.secrets = secrets;
        sm.data.version += 1;
        write_meta(&self.db, KEY_STATE_MACHINE, &*sm)?;
        Ok(())
    }

    /// Shared part of `force_quorum`/`emergency`: append a synthetic committed
    /// membership entry making `id` the sole voter (see `force_quorum`). Updates
    /// `sm`'s membership/log fields but does not persist; the caller writes once.
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
        write_meta(
            &self.db,
            KEY_VOTE,
            &crate::types::Vote::new_committed(term, id),
        )?;

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
        // `RB` isn't 'static, so it cannot move into `spawn_blocking`; owned bounds can.
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
            let purged: Option<LogId> = read_meta(&db, KEY_PURGED)?
                .map(|raw| decode(&raw))
                .transpose()?;

            let txn = db.begin_read().map_err(read_err)?;
            let table = txn.open_table(LOGS).map_err(read_err)?;
            let last = match table.last().map_err(read_err)? {
                Some((_, value)) => {
                    let entry: Entry = decode(value.value())?;
                    Some(entry.log_id)
                }
                None => purged,
            };

            Ok(LogState {
                last_purged_log_id: purged,
                last_log_id: last,
            })
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
        blocking(move || {
            read_meta(&db, KEY_VOTE)?
                .map(|raw| decode(&raw))
                .transpose()
        })
        .await
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

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), StorageError>
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
                table
                    .retain_in(log_id.index.., |_, _| false)
                    .map_err(write_err)?;
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
                meta.insert(KEY_PURGED, encoded.as_slice())
                    .map_err(write_err)?;
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
        // A snapshot this node cannot install MUST fail the install. Returning `Ok(())`
        // without touching state (as this once did) makes openraft advance
        // `last_applied`, purge the log and apply later entries on stale state,
        // permanently diverging this node.
        //
        // Trade-off: openraft 0.9 has no retriable error here; any `StorageError` is
        // fatal to the core (the leader re-sends after restart). A stopped, loudly
        // failing node that needs an upgrade beats one serving wrong state.
        let restored = match load_state_machine(&data)? {
            LoadedStateMachine::Current(sm) => *sm,
            LoadedStateMachine::TooNew { found } => {
                tracing::error!(
                    found,
                    current = crate::state::CURRENT_SCHEMA_VERSION,
                    "refusing a snapshot written by a newer schema than this build understands - update this node"
                );
                return Err(StorageIOError::<NodeId>::read(AnyError::error(format!(
                    "snapshot is schema_version {found}, newer than this build's {} - upgrade this node to install it",
                    crate::state::CURRENT_SCHEMA_VERSION
                )))
                .into());
            }
        };
        if restored.last_applied != meta.last_log_id {
            return Err(StorageIOError::<NodeId>::read(AnyError::error(format!(
                "snapshot data is applied through {:?} but its metadata says {:?}; refusing to install an inconsistent snapshot",
                restored.last_applied, meta.last_log_id
            )))
            .into());
        }
        let mut restored = restored;
        restored.schema_version = crate::state::CURRENT_SCHEMA_VERSION;

        // Hold the write lock across the disk write so a concurrent `build_snapshot`
        // cannot store an older snapshot over this one; state machine and snapshot
        // go in one transaction.
        let mut sm = self.sm.write().await;
        let db = self.db.clone();
        let stored = StoredSnapshot {
            meta: meta.clone(),
            data,
        };
        let sm_copy = restored.clone();
        blocking(move || {
            write_metas(
                &db,
                &[
                    (KEY_STATE_MACHINE, encode(&sm_copy)?),
                    (KEY_SNAPSHOT, encode(&stored)?),
                ],
            )
        })
        .await?;
        *sm = restored;
        Ok(())
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
        // The read lock is held across the disk write: `install_snapshot` takes the
        // write lock, so an older snapshot cannot overwrite a newer installed one.
        let sm = self.sm.read().await;
        let data = encode(&*sm)?;
        let meta = SnapshotMeta {
            last_log_id: sm.last_applied,
            last_membership: sm.membership.clone(),
            // The version just distinguishes successive snapshots.
            snapshot_id: format!(
                "{}-{}",
                sm.last_applied.map(|id| id.index).unwrap_or(0),
                sm.data.version
            ),
        };

        let db = self.db.clone();
        let stored = StoredSnapshot {
            meta: meta.clone(),
            data: data.clone(),
        };
        blocking(move || write_meta(&db, KEY_SNAPSHOT, &stored)).await?;
        drop(sm);

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
    serde_json::from_slice(raw)
        .map_err(|e| StorageIOError::<NodeId>::read(AnyError::new(&e)).into())
}

enum LoadedStateMachine {
    Current(Box<StateMachine>),
    /// `schema_version` on the blob is newer than `state::CURRENT_SCHEMA_VERSION`;
    /// no migration can apply. Callers decide how to react.
    TooNew {
        found: u32,
    },
}

/// Peeks a blob's `schema_version`, runs needed migrations (see
/// `crate::migrations`), then decodes normally.
fn load_state_machine(raw: &[u8]) -> Result<LoadedStateMachine, StorageError> {
    let value: serde_json::Value = serde_json::from_slice(raw)
        .map_err(|e| StorageError::from(StorageIOError::<NodeId>::read(AnyError::new(&e))))?;
    let found = value
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    if found > crate::state::CURRENT_SCHEMA_VERSION {
        return Ok(LoadedStateMachine::TooNew { found });
    }
    let migrated = crate::migrations::migrate_to_current(value, found);
    let mut sm: StateMachine = serde_json::from_value(migrated)
        .map_err(|e| StorageError::from(StorageIOError::<NodeId>::read(AnyError::new(&e))))?;
    // Stamp so it is never migrated twice.
    sm.schema_version = crate::state::CURRENT_SCHEMA_VERSION;
    Ok(LoadedStateMachine::Current(Box::new(sm)))
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

fn write_metas(db: &Database, entries: &[(&str, Vec<u8>)]) -> Result<(), StorageError> {
    let txn = db.begin_write().map_err(write_err)?;
    {
        let mut table = txn.open_table(META).map_err(write_err)?;
        for (key, value) in entries {
            table.insert(*key, value.as_slice()).map_err(write_err)?;
        }
    }
    txn.commit().map_err(write_err)
}

fn read_err<E: std::error::Error + 'static>(e: E) -> StorageError {
    StorageIOError::<NodeId>::read(AnyError::new(&e)).into()
}

fn write_err<E: std::error::Error + 'static>(e: E) -> StorageError {
    StorageIOError::<NodeId>::write(AnyError::new(&e)).into()
}

/// Runs a redb operation on the blocking pool. `commit()` fsyncs, which on slow
/// SD/USB storage can exceed the raft heartbeat soft ttl; inline on the async
/// task it would stall unrelated heartbeat/vote RPCs on the same executor and
/// show up as a bogus "Unreachable" timeout.
async fn blocking<T, F>(f: F) -> Result<T, StorageError>
where
    F: FnOnce() -> Result<T, StorageError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(write_err)?
}
