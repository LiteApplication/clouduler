//! The replicated cluster state and the commands that mutate it.
//!
//! Everything here is pure: `apply` is the only way state changes, and it never
//! performs IO. That keeps the state machine deterministic across nodes and
//! makes the invariants unit testable.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ids::{InstanceId, VolumeId};

/// Whether a node may take new work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Availability {
    /// Normal, schedulable.
    Active,
    /// Keeps running what it has, takes nothing new. Set before a migration.
    Drain,
    /// Removed from the cluster, holds nothing.
    Decommissioned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Manager,
    Worker,
}

/// A node as the cluster currently sees it. Seeded from the config file, then
/// edited at runtime through the web ui.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeState {
    pub name: String,
    /// Config-declared identity (`cl_config::Node::id`) this roster entry was
    /// created for. See the `removed` field on `ClusterState`.
    pub id: uuid::Uuid,
    pub role: Role,
    pub rpc_addr: SocketAddr,
    /// Drives this node implements, mapped to its local backing directory.
    pub drives: BTreeMap<String, PathBuf>,
    pub availability: Availability,
    /// Labels from the cluster config, replicated so the scheduler can match
    /// `node.labels.<key>` constraints without reading the config file.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Egress-kind networks this node has a host interface address for. Used
    /// the same way `drives` is: a service on one of these networks can only
    /// be placed on a node that implements it.
    #[serde(default)]
    pub egress_networks: BTreeSet<String>,
    /// Every network this node has any address on, keyed by network name.
    /// Used the same way `drives` is: a service attached to one of these
    /// networks can only be placed on a node that implements it.
    #[serde(default)]
    pub networks: BTreeSet<String>,
    /// Ports this node's public-network router forwards to it, keyed by
    /// network name. A raw `x-clouduler.expose` port on a public network can
    /// only be placed on a node that lists that `(protocol, port)` pair here.
    #[serde(default)]
    pub open_ports: BTreeMap<String, BTreeSet<(PortProtocol, u16)>>,
    /// `std::env::consts::ARCH` of the running binary, e.g. `x86_64` or
    /// `aarch64`. Lets the self-update orchestrator group nodes by what needs
    /// a native build.
    #[serde(default)]
    pub arch: String,
    /// Blake3 hash of the currently running binary, hex encoded. Lets the
    /// self-update orchestrator confirm a restarted node actually came back
    /// on the freshly built binary rather than just coming back.
    #[serde(default)]
    pub binary_hash: String,
    /// This node's X25519 public key, base64, self-reported once its local
    /// keypair exists. `None` until then. Lets any node holding the cluster
    /// secret key wrap a copy for this one without ever transmitting the
    /// plaintext key itself — see `wrapped_keys` on `ClusterState`.
    #[serde(default)]
    pub node_pubkey: Option<String>,
}

impl NodeState {
    pub fn implements(&self, drive: &str) -> bool {
        self.drives.contains_key(drive)
    }

    pub fn implements_egress_network(&self, network: &str) -> bool {
        self.egress_networks.contains(network)
    }

    pub fn implements_network(&self, network: &str) -> bool {
        self.networks.contains(network)
    }

    pub fn opens_port(&self, network: &str, protocol: PortProtocol, port: u16) -> bool {
        self.open_ports
            .get(network)
            .is_some_and(|ports| ports.contains(&(protocol, port)))
    }

    /// A drained or decommissioned node must not receive new containers.
    pub fn schedulable(&self) -> bool {
        self.availability == Availability::Active
    }
}

/// Transport protocol for a port a node's router forwards to it. Mirrors
/// `cl_config::stack::Protocol`, redeclared here so `cl-raft` does not depend
/// on `cl-config` (see `ClusterConfigEntry` below for the same reasoning).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortProtocol {
    Tcp,
    Udp,
}

/// A deployed stack. The spec is kept as the raw yaml the user submitted so the
/// ui can round trip it without lossy reserialisation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StackState {
    pub name: String,
    pub spec: String,
    /// Bumped on every redeploy, so nodes can tell a spec change from a restart.
    pub revision: u64,
    /// Cluster config objects this stack brought with it, so removing the stack
    /// takes them away too. A subset of `used_configs`: only the ones this
    /// stack bundled a `file:` for, not every config a service references.
    #[serde(default)]
    pub configs: BTreeSet<String>,
    /// Cluster secrets this stack brought with it. Same ownership caveat as
    /// `configs`.
    #[serde(default)]
    pub secrets: BTreeSet<String>,
    /// Every cluster config any service in this stack references, whether or
    /// not the stack bundles its own file for it. Kept alongside `configs`
    /// so the ui can show a stack's full config usage without the daemon
    /// having to reparse the spec.
    #[serde(default)]
    pub used_configs: BTreeSet<String>,
    /// Every cluster secret any service in this stack references. Same
    /// relationship to `secrets` as `used_configs` has to `configs`.
    #[serde(default)]
    pub used_secrets: BTreeSet<String>,
}

/// The cluster config file, replicated so an edit made on one node reaches the
/// file on disk on every node.
///
/// The file is what each daemon reads at startup, and a node that was down
/// during an edit would otherwise come back with a stale one. Held as the raw
/// yaml rather than a parsed `ClusterConfig` so `cl-raft` does not depend on
/// `cl-config`, and so comments and key order survive a round trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterConfigEntry {
    pub yaml: String,
    /// Bumped on every accepted edit. A node writes the file only when this
    /// moves, so a restarted daemon does not fight the operator over an
    /// unchanged file.
    pub generation: u64,
}

/// A cluster wide config object, mounted read only into the containers that
/// reference it. Stored in the clear: a config is not a secret.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigEntry {
    pub name: String,
    /// Base64, so the state machine stays valid json.
    pub content: String,
    /// Bumped on every update, so a node can tell its mounted copy is stale.
    pub revision: u64,
    /// Stack that owns it, if it came from a stack file rather than the ui.
    #[serde(default)]
    pub owner: Option<String>,
}

impl ConfigEntry {
    pub fn decode(&self) -> Result<Vec<u8>, String> {
        base64_decode(&self.content)
    }

    pub fn len(&self) -> usize {
        self.content.len() / 4 * 3
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
}

/// A cluster wide secret, mounted at `/run/secrets/<name>` like a docker secret.
///
/// The value is sealed before it reaches here, so the database, its snapshots
/// and any backup of them carry only ciphertext. A node without the cluster key
/// replicates this fine but cannot open it, and so cannot run a container that
/// needs it.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretEntry {
    pub name: String,
    /// Base64 of nonce followed by ciphertext, from `cl_secret::Sealed`.
    pub sealed: String,
    pub revision: u64,
    /// Fingerprint of the key it was sealed under, so a node holding the wrong
    /// key can say so instead of reporting a decryption failure.
    pub key_fingerprint: String,
    #[serde(default)]
    pub owner: Option<String>,
}

/// Prints no ciphertext, on the principle that nothing about a secret belongs
/// in a log line.
impl fmt::Debug for SecretEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretEntry")
            .field("name", &self.name)
            .field("revision", &self.revision)
            .field("key_fingerprint", &self.key_fingerprint)
            .finish_non_exhaustive()
    }
}

/// The cluster secret key, encrypted to one node's public key so only that
/// node can recover it. Written by whichever node currently holds the
/// plaintext key; the plaintext itself never appears here or anywhere else in
/// this state machine.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct WrappedKeyEntry {
    /// Base64 of the one-time ephemeral public key used for this wrap, from
    /// `cl_secret::WrappedKey::ephemeral_public`.
    pub ephemeral_public: String,
    /// Base64 of nonce followed by ciphertext, from `cl_secret::Sealed`.
    pub sealed: String,
    /// Fingerprint of the key that was wrapped, for observability.
    pub key_fingerprint: String,
    /// The recipient public key this was wrapped for. Compared against the
    /// node's current `node_pubkey` to notice a keypair rotation and
    /// re-wrap, rather than serving a copy nobody can open any more.
    pub target_pubkey: String,
}

/// Prints no ciphertext, same reasoning as `SecretEntry`'s `Debug`.
impl fmt::Debug for WrappedKeyEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WrappedKeyEntry")
            .field("key_fingerprint", &self.key_fingerprint)
            .field("target_pubkey", &self.target_pubkey)
            .finish_non_exhaustive()
    }
}

/// Cap on one config or secret. Same reasoning as the stack file limit: it goes
/// through the raft log and into every snapshot.
pub const MAX_OBJECT_BYTES: usize = 1024 * 1024;

/// Config and secret names become filenames under the mount directory, so they
/// have to be safe to join onto a path.
fn validate_object_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 253 {
        return Err(format!("{name:?} must be between 1 and 253 characters"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(format!(
            "{name:?} may only contain letters, digits, dot, dash and underscore"
        ));
    }
    if name.starts_with('.') {
        return Err(format!("{name:?} must not start with a dot"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DesiredState {
    Running,
    Stopped,
}

/// Where an instance runs and what it should be doing there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    pub instance: InstanceId,
    pub node: String,
    pub desired: DesiredState,
    /// Pinned image digest. Set once pulled so every replica runs the same bits
    /// even when the tag is `:latest`.
    pub image_digest: Option<String>,
    /// Present while the instance is moving to another node.
    pub migration: Option<Migration>,
}

/// A move between nodes, recorded step by step so a crash mid-move is recoverable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Migration {
    /// The node driving the move, which is where the instance started.
    ///
    /// Recorded rather than derived from the assignment because `Switching`
    /// repoints the assignment at the target: from that phase on there is no
    /// other way to tell who owns the remaining work, and a node that crashed
    /// mid-move has to be able to recognise its own job when it comes back.
    pub from: String,
    pub to: String,
    pub phase: MigrationPhase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MigrationPhase {
    /// Container is being stopped on the source node.
    Stopping,
    /// Final volume delta is being sent to the target.
    Syncing,
    /// Target is checking what it received.
    Verifying,
    /// Assignment is being switched over.
    Switching,
    /// Container is starting on the target.
    Starting,
    Done,
    Failed { reason: String },
}

impl MigrationPhase {
    /// The phase this one advances to, or None when the move is over.
    pub fn next(&self) -> Option<MigrationPhase> {
        use MigrationPhase::*;
        match self {
            Stopping => Some(Syncing),
            Syncing => Some(Verifying),
            Verifying => Some(Switching),
            Switching => Some(Starting),
            Starting => Some(Done),
            Done | Failed { .. } => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, MigrationPhase::Done | MigrationPhase::Failed { .. })
    }
}

/// Replication bookkeeping for one volume.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeState {
    pub id: VolumeId,
    /// Bumped by whichever node last wrote the volume. Lets a node tell whether
    /// its copy is behind without hashing the whole tree.
    pub generation: u64,
    /// Node holding the authoritative copy.
    pub primary: String,
    /// Generation each node has caught up to.
    pub replicas: BTreeMap<String, u64>,
}

impl VolumeState {
    /// Nodes whose copy is current.
    pub fn in_sync(&self) -> BTreeSet<&str> {
        self.replicas
            .iter()
            .filter(|(_, gen)| **gen == self.generation)
            .map(|(node, _)| node.as_str())
            .collect()
    }
}

/// A certificate clouduler holds, distributed to every node so any of them can
/// terminate TLS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CertificateState {
    pub fqdn: String,
    pub chain_pem: String,
    pub key_pem: String,
    /// Unix seconds.
    pub not_after: i64,
}

/// A DNS record clouduler created, tracked so ownership survives leader change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DnsRecordState {
    pub fqdn: String,
    pub record_type: String,
    pub content: String,
    pub zone: String,
    /// Cloudflare's id, so the record can be updated or deleted later.
    pub provider_id: Option<String>,
}

/// Where a certificate or DNS record stands before it has anything to show
/// for itself in `certificates`/`dns_records` - set as soon as a hostname
/// becomes desired, so the ui has something to render instead of silence
/// between "stack created" and "record live"/"cert issued".  Cleared (the
/// map entry removed) once the real record/certificate lands, so presence
/// in `certificates`/`dns_records` alone is always the "ready" case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReconcileStatus {
    Pending,
    Error {
        message: String,
        /// Unix seconds, so the ui can show how stale the failure is.
        at: i64,
    },
}

/// A place restic can keep a drive's backup history. A drive may back up to
/// several of these at once - see `DriveBackupTarget`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupDestination {
    pub id: String,
    pub name: String,
    pub kind: BackupDestinationKind,
    /// Name of the cluster secret holding the restic repo password. Always
    /// set: restic requires one regardless of backend.
    pub password_secret: String,
    /// Name of the cluster secret holding backend credentials (an ssh
    /// private key for `Ssh`, an access/secret key pair for `S3`). `None`
    /// for `NodeLocal`, which needs none of its own.
    #[serde(default)]
    pub credentials_secret: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "kind")]
pub enum BackupDestinationKind {
    /// A path on one cluster node's own disk. If that node does not already
    /// hold the drive being backed up, the runner syncs the data there once
    /// before running restic locally against it, rather than adding the node
    /// as a standing replica - a live mirror and a point-in-time history are
    /// different concerns and conflating them would mean every backup
    /// destination permanently costs ongoing replication traffic.
    NodeLocal { node: String, path: String },
    Ssh {
        host: String,
        port: u16,
        user: String,
        path: String,
    },
    S3 {
        bucket: String,
        endpoint: String,
        #[serde(default)]
        region: Option<String>,
        #[serde(default)]
        prefix: Option<String>,
    },
}

/// One drive's backup relationship with one destination: how often, what
/// time, and how long to keep it. A drive may have several of these, one per
/// destination it backs up to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveBackupTarget {
    pub id: String,
    pub drive: String,
    pub destination_id: String,
    pub enabled: bool,
    /// How often to run, in seconds.
    pub frequency_secs: u64,
    /// "HH:MM", 24h, node-local time. `None` means "any time of day once
    /// `frequency_secs` has elapsed", i.e. plain interval scheduling.
    #[serde(default)]
    pub time_of_day: Option<String>,
    #[serde(default)]
    pub keep_last: Option<u32>,
    #[serde(default)]
    pub keep_within_secs: Option<u64>,
    /// Overrides the resolved x-clouduler stop-before-backup default when
    /// set. `None` defers to the stack/service override, then a hardcoded
    /// `true` (see `cl_config::stack::StackFile::effective_backup_stop_before`).
    #[serde(default)]
    pub stop_containers: Option<bool>,
    /// Unix seconds this target last *started* a run, so the scheduler can
    /// tell whether `frequency_secs` has elapsed. `None` before its first run.
    #[serde(default)]
    pub last_run: Option<i64>,
}

/// One completed, failed, or in-flight backup run, kept as history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupRun {
    pub id: String,
    pub target_id: String,
    pub drive: String,
    pub destination_id: String,
    pub started_at: i64,
    #[serde(default)]
    pub finished_at: Option<i64>,
    pub outcome: BackupRunOutcome,
    pub triggered_by: BackupTrigger,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum BackupRunOutcome {
    /// `percent` is restic's own `percent_done` from its last `--json`
    /// status line, updated in place by repeated `RecordBackupRun` submits
    /// against the same run id while it executes - see
    /// `clouduler::backup_runner::run_restic_backup`. `None` until the first
    /// status line lands.
    Running {
        #[serde(default)]
        percent: Option<f64>,
    },
    Succeeded { snapshot_id: String, bytes: u64 },
    Failed { error: String },
}

// Hand-written so a raft log/snapshot written before `Running` grew its
// `percent` field (a bare unit variant, serialized as the JSON string
// `"Running"`) still replays: the derive-generated `Deserialize` rejects
// that shape outright, which crashes every node on startup the moment any
// node's log has ever recorded a pre-upgrade `Running` outcome. Route
// through `serde_json::Value` (works under any format, not just JSON) so the
// legacy string can be special-cased before falling back to the current
// struct-variant shape for everything else.
impl<'de> Deserialize<'de> for BackupRunOutcome {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        enum Current {
            Running {
                #[serde(default)]
                percent: Option<f64>,
            },
            Succeeded {
                snapshot_id: String,
                bytes: u64,
            },
            Failed {
                error: String,
            },
        }

        let value = serde_json::Value::deserialize(deserializer)?;
        if value.as_str() == Some("Running") {
            return Ok(BackupRunOutcome::Running { percent: None });
        }
        match serde_json::from_value(value).map_err(serde::de::Error::custom)? {
            Current::Running { percent } => Ok(BackupRunOutcome::Running { percent }),
            Current::Succeeded { snapshot_id, bytes } => Ok(BackupRunOutcome::Succeeded { snapshot_id, bytes }),
            Current::Failed { error } => Ok(BackupRunOutcome::Failed { error }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BackupTrigger {
    Schedule,
    Manual { by: String },
}

/// Newest runs kept per `(drive, destination_id)` pair. Same reasoning as
/// `MAX_OBJECT_BYTES`: history goes through the raft log and into every
/// snapshot, so it needs a bound even though nothing here is user-editable.
pub const MAX_BACKUP_RUNS_PER_TARGET: usize = 50;

/// What a restore request restores: a whole drive, one volume, or every
/// volume a service uses. Mirrors `backup_restore`'s three granularities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RestoreScope {
    Drive { drive: String },
    Volume { volume: VolumeId },
    Service { stack: String, service: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RestoreStatus {
    Pending,
    Running,
    Succeeded,
    Failed { error: String },
}

/// A restore triggered through the api (the CLI's `backup restore-*`
/// commands run directly against a node's own local drive registry/docker
/// and never go through this - see `backup_restore.rs`'s doc comment). This
/// exists so an api handler on any node can ask for a restore without
/// itself having drive/docker access: whichever node actually implements the
/// target drive claims it (`Pending` -> `Running`) and executes, the same
/// "submit an intent through raft, the node that can act on it does"
/// pattern `BackupRun` uses for scheduled/manual backups.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestoreRequest {
    pub id: String,
    pub scope: RestoreScope,
    /// Destination to restore from. Required for `Drive`/`Volume`; optional
    /// for `Service`, where it can be resolved automatically if the drive
    /// backs up to only one destination (see `backup_restore::resolve_service_destination`).
    #[serde(default)]
    pub destination_id: Option<String>,
    /// Snapshot id, or `None`/`"latest"` for the most recent one.
    #[serde(default)]
    pub snapshot: Option<String>,
    pub requested_at: i64,
    #[serde(default)]
    pub finished_at: Option<i64>,
    pub status: RestoreStatus,
    pub requested_by: String,
}

/// Newest requests kept. Restores are rare enough that, unlike backup runs,
/// one bound across the whole set (not per drive/destination) is simplest.
pub const MAX_RESTORE_REQUESTS: usize = 50;

/// The schema version this build's `ClusterState`/`Request` shapes are at.
/// Bump by hand whenever a change ships that an older node's persisted data
/// needs a `crate::migrations` entry to read, or that is otherwise unsafe
/// for a node behind this version to have installed - see `store.rs`'s use
/// of this alongside `StateMachine::schema_version`.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Everything the cluster agrees on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClusterState {
    pub nodes: BTreeMap<String, NodeState>,
    pub stacks: BTreeMap<String, StackState>,
    pub assignments: BTreeMap<InstanceId, Assignment>,
    pub volumes: BTreeMap<VolumeId, VolumeState>,
    /// The cluster config file, so every node can keep its own copy current.
    #[serde(default)]
    pub cluster_config: Option<ClusterConfigEntry>,
    /// Cluster wide config objects, mounted read only into containers.
    #[serde(default)]
    pub configs: BTreeMap<String, ConfigEntry>,
    /// Cluster wide secrets, sealed. Mounted at /run/secrets/<name>.
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretEntry>,
    pub certificates: BTreeMap<String, CertificateState>,
    pub dns_records: BTreeMap<String, DnsRecordState>,
    /// Pending/error status for a hostname that has no entry in
    /// `certificates` yet, keyed by fqdn. See `ReconcileStatus`.
    #[serde(default)]
    pub cert_status: BTreeMap<String, ReconcileStatus>,
    /// Pending/error status for a hostname that has no live record in
    /// `dns_records` yet, keyed by fqdn. See `ReconcileStatus`.
    #[serde(default)]
    pub dns_status: BTreeMap<String, ReconcileStatus>,
    /// This cluster's secret key, wrapped once per node, keyed by node name.
    /// See `WrappedKeyEntry`.
    #[serde(default)]
    pub wrapped_keys: BTreeMap<String, WrappedKeyEntry>,
    /// Nodes the leader has stopped hearing from. Observed rather than
    /// configured, so it is kept apart from `availability`, which is what the
    /// operator asked for.
    #[serde(default)]
    pub unreachable: BTreeSet<String>,
    /// Ids (`cl_config::Node::id`) removed through `RemoveNode`, kept as a
    /// tombstone forever. Without this, a leader whose in-memory config still
    /// lists the node (it is only ever reloaded from disk on restart) treats
    /// it as newly joined the next time it seeds the roster, undoing the
    /// removal. Keyed by id rather than name so the name is free to reuse: a
    /// node re-provisioned under the same name gets a fresh id and was never
    /// in this set.
    #[serde(default)]
    pub removed: BTreeSet<uuid::Uuid>,
    /// Overlay subnet and address assignments, as json owned by cl-net. Kept
    /// opaque here so the address book can evolve without touching the state
    /// machine, which every node has to agree on byte for byte.
    #[serde(default)]
    pub overlay: Option<String>,
    /// Set by `Store::emergency`'s offline recovery write, never by a normal
    /// `Request`: while this holds a stack name, the scheduler places and
    /// starts only that stack's instances, leaving every other assignment's
    /// `desired` untouched rather than forcing it to `Stopped` - so nothing
    /// has to be restored by hand once this clears. `ClearEmergency` is the
    /// only way out, submitted by `watch_leadership` the moment this node can
    /// once again reach another voter (see `QuorumStatus::reachable`), which
    /// is the cluster's own signal that another manager has come back.
    #[serde(default)]
    pub emergency_stack: Option<String>,
    /// Where a drive can back up to. See `BackupDestination`.
    #[serde(default)]
    pub restic_destinations: BTreeMap<String, BackupDestination>,
    /// A drive's schedule/retention against one destination. See
    /// `DriveBackupTarget`.
    #[serde(default)]
    pub restic_targets: BTreeMap<String, DriveBackupTarget>,
    /// Backup run history, bounded per target - see `MAX_BACKUP_RUNS_PER_TARGET`.
    #[serde(default)]
    pub restic_runs: BTreeMap<String, BackupRun>,
    /// Restore requests submitted through the api, bounded - see
    /// `MAX_RESTORE_REQUESTS`.
    #[serde(default)]
    pub restore_requests: BTreeMap<String, RestoreRequest>,
    /// Incremented on every applied command. Cheap way for watchers to notice
    /// a change without diffing.
    pub version: u64,
}

/// A command submitted through raft. Every mutation of cluster state is one of these.
///
/// `Deserialize` is hand-written below rather than derived - see the impl for
/// why: an unrecognized variant tag (from a node running newer code) must
/// fall back to `Unknown` instead of failing outright, both for local log/
/// snapshot reads and for this enum's use as the wire payload for raft
/// replication itself.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Request {
    /// Replace the cluster config file on every node.
    ///
    /// `expect_generation` is the generation the writer read before editing, 0
    /// when nothing was stored yet. A mismatch means someone else edited in
    /// between and the command is refused rather than silently overwriting
    /// their change.
    PutClusterConfig {
        yaml: String,
        expect_generation: u64,
    },
    UpsertNode(NodeState),
    SetAvailability {
        node: String,
        availability: Availability,
    },
    /// Submitted by the leader once a node has been silent for longer than
    /// `failover.node_timeout`, or once it answers again.
    SetReachable {
        node: String,
        reachable: bool,
    },
    RemoveNode {
        node: String,
    },
    /// Tombstone an id without touching the roster, so `reap_removed_nodes`
    /// drops it from raft membership on its own timer.
    ///
    /// For a roster entry that changes identity while keeping its name: a
    /// node re-provisioned fresh (`clouduler init --force`) after its old
    /// state was lost, or one `force_quorum` had demoted to a learner rather
    /// than dropped, coming back under a new id through `init --join`.
    /// `RemoveNode` does not fit this case, since it deletes the name's
    /// whole roster entry, which here still points at a live node under its
    /// new id.
    RetireNodeId {
        id: uuid::Uuid,
    },
    DeployStack {
        name: String,
        spec: String,
    },
    RemoveStack {
        name: String,
    },
    /// Record which cluster objects a stack owns, so removing the stack cleans
    /// them up. Sent after the objects themselves have been written.
    SetStackObjects {
        name: String,
        configs: BTreeSet<String>,
        secrets: BTreeSet<String>,
        #[serde(default)]
        used_configs: BTreeSet<String>,
        #[serde(default)]
        used_secrets: BTreeSet<String>,
    },
    /// Place an instance. `drives` lists what the instance needs, so the state
    /// machine can refuse a node that does not implement all of them.
    Assign {
        instance: InstanceId,
        node: String,
        drives: BTreeSet<String>,
    },
    Unassign {
        instance: InstanceId,
    },
    SetDesiredState {
        instance: InstanceId,
        desired: DesiredState,
    },
    SetImageDigest {
        instance: InstanceId,
        digest: String,
    },
    BeginMigration {
        instance: InstanceId,
        to: String,
        drives: BTreeSet<String>,
    },
    AdvanceMigration {
        instance: InstanceId,
        phase: MigrationPhase,
    },
    UpsertVolume {
        id: VolumeId,
        primary: String,
    },
    /// Records that `node` has replicated the volume up to `generation`.
    SetVolumeReplica {
        id: VolumeId,
        node: String,
        generation: u64,
    },
    /// Bumps the generation, marking every other replica stale.
    TouchVolume {
        id: VolumeId,
        primary: String,
    },
    StoreCertificate(CertificateState),
    RemoveCertificate {
        fqdn: String,
    },
    /// Records that a hostname's certificate is being ordered, or failed to
    /// be. Superseded by `StoreCertificate` once issuance succeeds.
    SetCertStatus {
        fqdn: String,
        status: ReconcileStatus,
    },
    UpsertDnsRecord(DnsRecordState),
    RemoveDnsRecord {
        key: String,
    },
    /// Records that a hostname's dns record is being reconciled, or failed
    /// to be. Superseded once `UpsertDnsRecord` lands a live record for it.
    SetDnsStatus {
        fqdn: String,
        status: ReconcileStatus,
    },
    PutConfig {
        name: String,
        /// Base64 content.
        content: String,
        #[serde(default)]
        owner: Option<String>,
    },
    RemoveConfig {
        name: String,
    },
    /// The value is sealed by the caller: plaintext never travels through the
    /// raft log, so it is never written to disk unencrypted.
    PutSecret {
        name: String,
        sealed: String,
        key_fingerprint: String,
        #[serde(default)]
        owner: Option<String>,
    },
    RemoveSecret {
        name: String,
    },
    /// Written by whichever node currently holds the plaintext cluster key,
    /// once it sees `node` publish a pubkey with no wrapped copy yet (or a
    /// stale one, after a keypair rotation).
    PutWrappedKey {
        node: String,
        entry: WrappedKeyEntry,
    },
    /// Overlay address book. Allocation goes through raft so two nodes cannot
    /// hand out the same address for the same stack network.
    SetOverlayAllocations {
        allocations: String,
    },
    PutBackupDestination(BackupDestination),
    RemoveBackupDestination {
        id: String,
    },
    PutBackupTarget(DriveBackupTarget),
    RemoveBackupTarget {
        id: String,
    },
    /// Submitted by the leader when it decides a target is due, claiming the
    /// tick so no other leader term double-fires it.
    SetBackupTargetLastRun {
        id: String,
        at: i64,
    },
    /// Insert or update-in-place by id: `Running` -> `Succeeded`/`Failed`.
    RecordBackupRun(BackupRun),
    /// Submitted by an api handler; always inserted with `status: Pending`
    /// regardless of what the caller sent, since only the node that goes on
    /// to claim and execute it may advance that.
    PutRestoreRequest(RestoreRequest),
    /// Advances one restore request's status: a claim (`Pending` ->
    /// `Running`), or a terminal outcome. The claim step doubles as an
    /// optimistic lock - if two nodes both think they should run the same
    /// request, only the first `SetRestoreStatus{Running}` to commit wins;
    /// the loser's own claim attempt fails because the status is no longer
    /// `Pending`, and it skips the request (see `backup_restore` module docs).
    SetRestoreStatus {
        id: String,
        status: RestoreStatus,
        /// Unix seconds, supplied by the caller rather than read from the
        /// clock here - `apply` must stay pure so every replica computes the
        /// same state from the same log. Only used (as `finished_at`) when
        /// `status` is terminal.
        at: i64,
    },
    /// Written on leader election so the log always has a fresh entry to commit.
    NoOp,
    /// Ends emergency mode: clears `ClusterState::emergency_stack`, letting
    /// every stack's own `desired` state (never touched by emergency) govern
    /// placement again. Submitted by `watch_leadership` once this node can
    /// reach another voter, not by an operator - see `emergency_stack`.
    ClearEmergency,
    /// Stand-in for a variant this node's binary doesn't recognize - a
    /// command a newer node submitted that this (older) node hasn't been
    /// upgraded to understand yet. `dispatch` treats it as a no-op; the
    /// command's real effect lands once this node is upgraded and either
    /// replays a snapshot the (by-then-migrated) leader sends, or is itself
    /// upgraded before this entry is compacted. Never constructed directly -
    /// only ever produced by `Deserialize` below.
    Unknown,
}

// Mirrors `Request` field-for-field purely so `#[derive(Deserialize)]` can
// generate the normal per-variant parsing logic under a different entry
// point (`serde(remote)`), which the impl below calls into. Every variant
// added to `Request` must be added here too, in the same shape - `dispatch`'s
// exhaustive match forces you to handle a new variant there, but nothing
// forces updating this mirror, and if it drifts a variant this build
// genuinely understands would incorrectly deserialize as `Unknown`. The
// `every_variant_name_round_trips` test below is the tripwire for that: it
// must be extended with each new variant.
mod request_mirror {
    use super::*;

    #[derive(Deserialize)]
    #[serde(remote = "Request")]
    pub enum RequestMirror {
        PutClusterConfig {
            yaml: String,
            expect_generation: u64,
        },
        UpsertNode(NodeState),
        SetAvailability {
            node: String,
            availability: Availability,
        },
        SetReachable {
            node: String,
            reachable: bool,
        },
        RemoveNode {
            node: String,
        },
        RetireNodeId {
            id: uuid::Uuid,
        },
        DeployStack {
            name: String,
            spec: String,
        },
        RemoveStack {
            name: String,
        },
        SetStackObjects {
            name: String,
            configs: BTreeSet<String>,
            secrets: BTreeSet<String>,
            #[serde(default)]
            used_configs: BTreeSet<String>,
            #[serde(default)]
            used_secrets: BTreeSet<String>,
        },
        Assign {
            instance: InstanceId,
            node: String,
            drives: BTreeSet<String>,
        },
        Unassign {
            instance: InstanceId,
        },
        SetDesiredState {
            instance: InstanceId,
            desired: DesiredState,
        },
        SetImageDigest {
            instance: InstanceId,
            digest: String,
        },
        BeginMigration {
            instance: InstanceId,
            to: String,
            drives: BTreeSet<String>,
        },
        AdvanceMigration {
            instance: InstanceId,
            phase: MigrationPhase,
        },
        UpsertVolume {
            id: VolumeId,
            primary: String,
        },
        SetVolumeReplica {
            id: VolumeId,
            node: String,
            generation: u64,
        },
        TouchVolume {
            id: VolumeId,
            primary: String,
        },
        StoreCertificate(CertificateState),
        RemoveCertificate {
            fqdn: String,
        },
        SetCertStatus {
            fqdn: String,
            status: ReconcileStatus,
        },
        UpsertDnsRecord(DnsRecordState),
        RemoveDnsRecord {
            key: String,
        },
        SetDnsStatus {
            fqdn: String,
            status: ReconcileStatus,
        },
        PutConfig {
            name: String,
            content: String,
            #[serde(default)]
            owner: Option<String>,
        },
        RemoveConfig {
            name: String,
        },
        PutSecret {
            name: String,
            sealed: String,
            key_fingerprint: String,
            #[serde(default)]
            owner: Option<String>,
        },
        RemoveSecret {
            name: String,
        },
        PutWrappedKey {
            node: String,
            entry: WrappedKeyEntry,
        },
        SetOverlayAllocations {
            allocations: String,
        },
        PutBackupDestination(BackupDestination),
        RemoveBackupDestination {
            id: String,
        },
        PutBackupTarget(DriveBackupTarget),
        RemoveBackupTarget {
            id: String,
        },
        SetBackupTargetLastRun {
            id: String,
            at: i64,
        },
        RecordBackupRun(BackupRun),
        PutRestoreRequest(RestoreRequest),
        SetRestoreStatus {
            id: String,
            status: RestoreStatus,
            at: i64,
        },
        NoOp,
        ClearEmergency,
        // `Unknown` is deliberately absent: there is no wire shape a sender
        // would legitimately produce for it - it is only ever the fallback
        // result when nothing above matches.
    }
}

// Hand-written for the same reason as `BackupRunOutcome`'s impl above - but
// here the concern is forward, not backward, compatibility: this must
// tolerate a variant tag it has never heard of (from a node running code
// newer than this build), not just an old shape of one it knows. Delegates
// the real parsing to `request_mirror::RequestMirror` (a field-for-field
// copy of `Request` under `#[serde(remote = "Request")]`, the standard serde
// idiom for "derive normally, but let me override the public impl") and
// falls back to `Request::Unknown` - rather than an error - when the tag
// doesn't match any variant that mirror knows. `dispatch` treats `Unknown`
// as a no-op: the sending node is newer, this one just hasn't caught up yet.
// This also covers raft replication itself, not just local log/snapshot
// reads: openraft's `Entry<TypeConfig>` embeds `Request` directly, so this
// impl is what protects an old follower from a newer leader's brand-new
// command variant.
impl<'de> Deserialize<'de> for Request {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match request_mirror::RequestMirror::deserialize(&value) {
            Ok(req) => Ok(req),
            Err(_) => Ok(Request::Unknown),
        }
    }
}

/// Result of applying a command. Failures are values, not errors: a rejected
/// command is still a committed log entry on every node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub version: u64,
    pub error: Option<String>,
}

impl Response {
    fn ok(version: u64) -> Self {
        Self {
            version,
            error: None,
        }
    }

    fn err(version: u64, msg: impl Into<String>) -> Self {
        Self {
            version,
            error: Some(msg.into()),
        }
    }

    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

impl ClusterState {
    /// Apply one command. Must stay deterministic, so no clocks, no randomness
    /// and no IO.
    pub fn apply(&mut self, req: &Request) -> Response {
        let result = self.dispatch(req);
        match result {
            Ok(()) => {
                self.version += 1;
                Response::ok(self.version)
            }
            // A rejected command leaves state untouched, so the version stays put.
            Err(msg) => Response::err(self.version, msg),
        }
    }

    fn dispatch(&mut self, req: &Request) -> Result<(), String> {
        match req {
            Request::NoOp => Ok(()),

            Request::Unknown => {
                tracing::warn!(
                    "received a Request variant this node's binary doesn't recognize (from an upgraded peer) - ignoring until this node is updated"
                );
                Ok(())
            }

            Request::PutClusterConfig {
                yaml,
                expect_generation,
            } => {
                let current = self
                    .cluster_config
                    .as_ref()
                    .map(|c| c.generation)
                    .unwrap_or(0);
                if current != *expect_generation {
                    return Err(format!(
                        "cluster config was edited concurrently (stored generation {current}, edit was made against {expect_generation})"
                    ));
                }
                self.cluster_config = Some(ClusterConfigEntry {
                    yaml: yaml.clone(),
                    generation: current + 1,
                });
                Ok(())
            }

            Request::PutConfig {
                name,
                content,
                owner,
            } => {
                validate_object_name(name)?;
                if base64_decode(content).is_err() {
                    return Err(format!("config {name} is not valid base64"));
                }
                let revision = self.configs.get(name).map(|c| c.revision + 1).unwrap_or(1);
                let entry = ConfigEntry {
                    name: name.clone(),
                    content: content.clone(),
                    revision,
                    owner: owner.clone(),
                };
                if entry.len() > MAX_OBJECT_BYTES {
                    return Err(format!(
                        "config {name} is over the {MAX_OBJECT_BYTES} byte limit"
                    ));
                }
                self.configs.insert(name.clone(), entry);
                Ok(())
            }

            Request::RemoveConfig { name } => {
                if let Some(user) = self.config_in_use(name) {
                    return Err(format!("config {name} is still used by {user}"));
                }
                self.configs.remove(name);
                Ok(())
            }

            Request::PutSecret {
                name,
                sealed,
                key_fingerprint,
                owner,
            } => {
                validate_object_name(name)?;
                if base64_decode(sealed).is_err() {
                    return Err(format!("secret {name} is not valid base64"));
                }
                let revision = self.secrets.get(name).map(|s| s.revision + 1).unwrap_or(1);
                let entry = SecretEntry {
                    name: name.clone(),
                    sealed: sealed.clone(),
                    revision,
                    key_fingerprint: key_fingerprint.clone(),
                    owner: owner.clone(),
                };
                if entry.sealed.len() / 4 * 3 > MAX_OBJECT_BYTES {
                    return Err(format!(
                        "secret {name} is over the {MAX_OBJECT_BYTES} byte limit"
                    ));
                }
                self.secrets.insert(name.clone(), entry);
                Ok(())
            }

            Request::RemoveSecret { name } => {
                if let Some(user) = self.secret_in_use(name) {
                    return Err(format!("secret {name} is still used by {user}"));
                }
                self.secrets.remove(name);
                Ok(())
            }

            Request::PutWrappedKey { node, entry } => {
                let current = self
                    .nodes
                    .get(node)
                    .ok_or_else(|| format!("unknown node {node}"))?;
                if current.node_pubkey.as_deref() != Some(entry.target_pubkey.as_str()) {
                    return Err(format!("node {node} has not published the public key this was wrapped for"));
                }
                self.wrapped_keys.insert(node.clone(), entry.clone());
                Ok(())
            }

            Request::SetOverlayAllocations { allocations } => {
                self.overlay = Some(allocations.clone());
                Ok(())
            }

            Request::ClearEmergency => {
                self.emergency_stack = None;
                Ok(())
            }

            Request::UpsertNode(node) => {
                self.removed.remove(&node.id);
                self.nodes.insert(node.name.clone(), node.clone());
                Ok(())
            }

            Request::SetAvailability { node, availability } => {
                let entry = self
                    .nodes
                    .get_mut(node)
                    .ok_or_else(|| format!("unknown node {node}"))?;
                entry.availability = *availability;
                Ok(())
            }

            Request::SetReachable { node, reachable } => {
                if !self.nodes.contains_key(node) {
                    return Err(format!("unknown node {node}"));
                }
                if *reachable {
                    self.unreachable.remove(node);
                } else {
                    self.unreachable.insert(node.clone());
                }
                Ok(())
            }

            Request::RemoveNode { node } => {
                if let Some(inst) = self.assignments.values().find(|a| &a.node == node) {
                    return Err(format!("node {node} still runs {}", inst.instance));
                }
                let removed = self.nodes.remove(node);
                self.unreachable.remove(node);
                self.wrapped_keys.remove(node);
                if let Some(removed) = removed {
                    self.removed.insert(removed.id);
                }
                for volume in self.volumes.values_mut() {
                    volume.replicas.remove(node);
                }
                Ok(())
            }

            Request::RetireNodeId { id } => {
                self.removed.insert(*id);
                Ok(())
            }

            Request::DeployStack { name, spec } => {
                let revision = self.stacks.get(name).map(|s| s.revision + 1).unwrap_or(1);
                let previous = self.stacks.get(name);
                let configs = previous.map(|s| s.configs.clone()).unwrap_or_default();
                let secrets = previous.map(|s| s.secrets.clone()).unwrap_or_default();
                let used_configs = previous.map(|s| s.used_configs.clone()).unwrap_or_default();
                let used_secrets = previous.map(|s| s.used_secrets.clone()).unwrap_or_default();
                self.stacks.insert(
                    name.clone(),
                    StackState {
                        name: name.clone(),
                        spec: spec.clone(),
                        revision,
                        configs,
                        secrets,
                        used_configs,
                        used_secrets,
                    },
                );
                Ok(())
            }

            Request::RemoveStack { name } => {
                // Drop the stack first so the in-use check below sees it gone.
                let removed = self.stacks.remove(name);
                self.assignments.retain(|id, _| &id.stack != name);
                if let Some(stack) = removed {
                    for config in &stack.configs {
                        if self.config_in_use(config).is_none() {
                            self.configs.remove(config);
                        }
                    }
                    for secret in &stack.secrets {
                        if self.secret_in_use(secret).is_none() {
                            self.secrets.remove(secret);
                        }
                    }
                }
                Ok(())
            }

            Request::SetStackObjects {
                name,
                configs,
                secrets,
                used_configs,
                used_secrets,
            } => {
                let stack = self
                    .stacks
                    .get_mut(name)
                    .ok_or_else(|| format!("unknown stack {name}"))?;
                stack.configs = configs.clone();
                stack.secrets = secrets.clone();
                stack.used_configs = used_configs.clone();
                stack.used_secrets = used_secrets.clone();
                Ok(())
            }

            Request::Assign {
                instance,
                node,
                drives,
            } => {
                self.check_placement(node, drives)?;
                // Carry over a pin from the assignment being replaced. An
                // instance moved to a different node by the scheduler (its
                // old node went unreachable, was drained, or stopped
                // implementing something it needs) is still running the same
                // pinned image - only where it runs changed - so losing the
                // pin here would let it silently drift back to whatever tag
                // it deploys from now resolves to the next time it restarts.
                let image_digest = self.assignments.get(instance).and_then(|a| a.image_digest.clone());
                self.assignments.insert(
                    instance.clone(),
                    Assignment {
                        instance: instance.clone(),
                        node: node.clone(),
                        desired: DesiredState::Running,
                        image_digest,
                        migration: None,
                    },
                );
                Ok(())
            }

            Request::Unassign { instance } => {
                self.assignments.remove(instance);
                Ok(())
            }

            Request::SetDesiredState { instance, desired } => {
                self.assignment_mut(instance)?.desired = *desired;
                Ok(())
            }

            Request::SetImageDigest { instance, digest } => {
                self.assignment_mut(instance)?.image_digest = Some(digest.clone());
                Ok(())
            }

            Request::BeginMigration {
                instance,
                to,
                drives,
            } => {
                self.check_placement(to, drives)?;
                let assignment = self.assignment_mut(instance)?;
                if assignment.node == *to {
                    return Err(format!("{instance} already runs on {to}"));
                }
                if let Some(active) = &assignment.migration {
                    if !active.phase.is_terminal() {
                        return Err(format!("{instance} is already migrating to {}", active.to));
                    }
                }
                assignment.migration = Some(Migration {
                    from: assignment.node.clone(),
                    to: to.clone(),
                    phase: MigrationPhase::Stopping,
                });
                Ok(())
            }

            Request::AdvanceMigration { instance, phase } => {
                let assignment = self.assignment_mut(instance)?;
                let migration = assignment
                    .migration
                    .as_mut()
                    .ok_or_else(|| format!("{instance} is not migrating"))?;

                // Asking for the phase already on record changes nothing, so
                // it succeeds rather than reading as an illegal transition. A
                // driver that retries a submit it never saw the answer to has
                // to be able to tell "already done" from "not allowed".
                if &migration.phase == phase {
                    return Ok(());
                }

                // Failure can interrupt at any point, but forward progress must
                // follow the phase order so a stale message cannot rewind a move.
                let allowed = matches!(phase, MigrationPhase::Failed { .. })
                    || migration.phase.next().as_ref() == Some(phase);
                if !allowed {
                    return Err(format!(
                        "cannot advance {instance} from {:?} to {phase:?}",
                        migration.phase
                    ));
                }

                migration.phase = phase.clone();
                match phase {
                    // Switching over is the point of no return: the target owns
                    // the instance from here.
                    MigrationPhase::Switching => assignment.node = migration.to.clone(),
                    MigrationPhase::Done => assignment.migration = None,
                    _ => {}
                }
                Ok(())
            }

            Request::UpsertVolume { id, primary } => {
                if !self.node_implements(primary, &id.drive) {
                    return Err(format!("node {primary} does not implement drive {}", id.drive));
                }
                self.volumes.entry(id.clone()).or_insert_with(|| VolumeState {
                    id: id.clone(),
                    generation: 1,
                    primary: primary.clone(),
                    replicas: BTreeMap::from([(primary.clone(), 1)]),
                });
                Ok(())
            }

            Request::SetVolumeReplica {
                id,
                node,
                generation,
            } => {
                let volume = self
                    .volumes
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown volume {id}"))?;
                if *generation > volume.generation {
                    return Err(format!(
                        "replica generation {generation} is ahead of {} for {id}",
                        volume.generation
                    ));
                }
                let entry = volume.replicas.entry(node.clone()).or_insert(0);
                // Sync results can arrive out of order, so never move a replica back.
                *entry = (*entry).max(*generation);
                Ok(())
            }

            Request::TouchVolume { id, primary } => {
                if !self.node_implements(primary, &id.drive) {
                    return Err(format!("node {primary} does not implement drive {}", id.drive));
                }
                let volume = self
                    .volumes
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown volume {id}"))?;
                volume.generation += 1;
                volume.primary = primary.clone();
                volume.replicas.insert(primary.clone(), volume.generation);
                Ok(())
            }

            Request::PutBackupDestination(destination) => {
                validate_object_name(&destination.id)?;
                if let BackupDestinationKind::NodeLocal { node, .. } = &destination.kind {
                    if !self.nodes.contains_key(node) {
                        return Err(format!("unknown node {node}"));
                    }
                }
                self.restic_destinations.insert(destination.id.clone(), destination.clone());
                Ok(())
            }

            Request::RemoveBackupDestination { id } => {
                if let Some(user) = self.destination_in_use(id) {
                    return Err(format!("backup destination {id} is still used by {user}"));
                }
                self.restic_destinations.remove(id);
                Ok(())
            }

            Request::PutBackupTarget(target) => {
                validate_object_name(&target.id)?;
                if !self.drive_known(&target.drive) {
                    return Err(format!("unknown drive {}", target.drive));
                }
                if !self.restic_destinations.contains_key(&target.destination_id) {
                    return Err(format!("unknown backup destination {}", target.destination_id));
                }
                if target.enabled
                    && self.restic_targets.values().any(|t| {
                        t.id != target.id
                            && t.enabled
                            && t.drive == target.drive
                            && t.destination_id == target.destination_id
                    })
                {
                    return Err(format!(
                        "drive {} already has an enabled backup target for destination {}",
                        target.drive, target.destination_id
                    ));
                }
                self.restic_targets.insert(target.id.clone(), target.clone());
                Ok(())
            }

            Request::RemoveBackupTarget { id } => {
                self.restic_targets.remove(id);
                Ok(())
            }

            Request::SetBackupTargetLastRun { id, at } => {
                let target = self
                    .restic_targets
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown backup target {id}"))?;
                target.last_run = Some(*at);
                Ok(())
            }

            Request::RecordBackupRun(run) => {
                // A progress/finish update against a run's own id is always
                // allowed (that run *is* the one already running); only a
                // brand-new run competing for the same target is rejected.
                if matches!(run.outcome, BackupRunOutcome::Running { .. })
                    && self
                        .restic_runs
                        .values()
                        .any(|r| r.target_id == run.target_id && r.id != run.id && matches!(r.outcome, BackupRunOutcome::Running { .. }))
                {
                    return Err(format!("a backup is already running for target {}", run.target_id));
                }
                self.restic_runs.insert(run.id.clone(), run.clone());
                let mut same_target: Vec<&BackupRun> = self
                    .restic_runs
                    .values()
                    .filter(|r| r.drive == run.drive && r.destination_id == run.destination_id)
                    .collect();
                if same_target.len() > MAX_BACKUP_RUNS_PER_TARGET {
                    same_target.sort_by_key(|r| r.started_at);
                    let drop_count = same_target.len() - MAX_BACKUP_RUNS_PER_TARGET;
                    let to_drop: Vec<String> = same_target.iter().take(drop_count).map(|r| r.id.clone()).collect();
                    for id in to_drop {
                        self.restic_runs.remove(&id);
                    }
                }
                Ok(())
            }

            Request::PutRestoreRequest(request) => {
                validate_object_name(&request.id)?;
                match &request.scope {
                    RestoreScope::Drive { drive } | RestoreScope::Volume { volume: VolumeId { drive, .. } } => {
                        if !self.drive_known(drive) {
                            return Err(format!("unknown drive {drive}"));
                        }
                    }
                    RestoreScope::Service { stack, .. } => {
                        if !self.stacks.contains_key(stack) {
                            return Err(format!("unknown stack {stack}"));
                        }
                    }
                }
                if let Some(destination_id) = &request.destination_id {
                    if !self.restic_destinations.contains_key(destination_id) {
                        return Err(format!("unknown backup destination {destination_id}"));
                    }
                }
                let mut request = request.clone();
                request.status = RestoreStatus::Pending;
                request.finished_at = None;
                self.restore_requests.insert(request.id.clone(), request);

                if self.restore_requests.len() > MAX_RESTORE_REQUESTS {
                    let mut all: Vec<&RestoreRequest> = self.restore_requests.values().collect();
                    all.sort_by_key(|r| r.requested_at);
                    let drop_count = all.len() - MAX_RESTORE_REQUESTS;
                    let to_drop: Vec<String> = all.iter().take(drop_count).map(|r| r.id.clone()).collect();
                    for id in to_drop {
                        self.restore_requests.remove(&id);
                    }
                }
                Ok(())
            }

            Request::SetRestoreStatus { id, status, at } => {
                let request = self
                    .restore_requests
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown restore request {id}"))?;
                if matches!(request.status, RestoreStatus::Succeeded | RestoreStatus::Failed { .. }) {
                    return Err(format!("restore request {id} already finished"));
                }
                if matches!(status, RestoreStatus::Running) && !matches!(request.status, RestoreStatus::Pending) {
                    return Err(format!("restore request {id} is not pending, cannot be claimed"));
                }
                if matches!(status, RestoreStatus::Succeeded | RestoreStatus::Failed { .. }) {
                    request.finished_at = Some(*at);
                }
                request.status = status.clone();
                Ok(())
            }

            Request::StoreCertificate(cert) => {
                self.cert_status.remove(&cert.fqdn);
                self.certificates.insert(cert.fqdn.clone(), cert.clone());
                Ok(())
            }

            Request::RemoveCertificate { fqdn } => {
                self.certificates.remove(fqdn);
                Ok(())
            }

            Request::SetCertStatus { fqdn, status } => {
                self.cert_status.insert(fqdn.clone(), status.clone());
                Ok(())
            }

            Request::UpsertDnsRecord(record) => {
                self.dns_status.remove(&record.fqdn);
                let key = dns_key(&record.fqdn, &record.record_type, &record.content);
                self.dns_records.insert(key, record.clone());
                Ok(())
            }

            Request::RemoveDnsRecord { key } => {
                self.dns_records.remove(key);
                Ok(())
            }

            Request::SetDnsStatus { fqdn, status } => {
                self.dns_status.insert(fqdn.clone(), status.clone());
                Ok(())
            }
        }
    }

    fn assignment_mut(&mut self, instance: &InstanceId) -> Result<&mut Assignment, String> {
        self.assignments
            .get_mut(instance)
            .ok_or_else(|| format!("{instance} is not assigned"))
    }

    fn node_implements(&self, node: &str, drive: &str) -> bool {
        self.nodes.get(node).is_some_and(|n| n.implements(drive))
    }

    /// Whether a node may take new work: the operator has not held it back and
    /// the cluster can still hear from it.
    pub fn can_schedule(&self, node: &str) -> bool {
        self.nodes.get(node).is_some_and(|n| n.schedulable()) && !self.unreachable.contains(node)
    }

    /// The core scheduling invariant: an instance only lands on a node that is
    /// taking work and implements every *replicated* drive its volumes live
    /// on. A served drive has one authoritative copy any node can reach over
    /// the internal network (mounted per volume, on demand, once the
    /// instance is actually placed), so it never narrows candidates this way
    /// — mirrors the same carve-out `cl_sched::place::rejection` applies
    /// when it picks a candidate in the first place; this is the second,
    /// independent check every node's own state machine runs on the
    /// resulting `PlaceInstance`/`MoveInstance` request, and the two have to
    /// agree or a served-drive placement the scheduler chose would always be
    /// rejected here.
    fn check_placement(&self, node: &str, drives: &BTreeSet<String>) -> Result<(), String> {
        let target = self
            .nodes
            .get(node)
            .ok_or_else(|| format!("unknown node {node}"))?;
        if !target.schedulable() {
            return Err(format!("node {node} is {:?}", target.availability));
        }
        if self.unreachable.contains(node) {
            return Err(format!("node {node} is unreachable"));
        }
        let served = self.served_drives();
        for drive in drives {
            if !served.contains(drive) && !target.implements(drive) {
                return Err(format!("node {node} does not implement drive {drive}"));
            }
        }
        Ok(())
    }

    /// Drive names whose mode is `served`, read from the replicated cluster
    /// config's raw yaml with a minimal shape local to this crate rather
    /// than `cl_config::ClusterConfig` (see `ClusterConfigEntry`'s doc
    /// comment for why `cl-raft` does not depend on `cl-config` for this).
    /// Empty if no config has been published yet or it fails to parse, same
    /// fallback `cl_sched::run::served_drives` uses for the config this is
    /// mirrored from.
    fn served_drives(&self) -> BTreeSet<String> {
        #[derive(Deserialize)]
        struct Shape {
            #[serde(default)]
            drives: BTreeMap<String, DriveShape>,
        }
        #[derive(Deserialize, Default)]
        struct DriveShape {
            #[serde(default)]
            mode: String,
        }
        let Some(entry) = &self.cluster_config else {
            return BTreeSet::new();
        };
        let Ok(shape) = serde_yaml::from_str::<Shape>(&entry.yaml) else {
            return BTreeSet::new();
        };
        shape.drives.into_iter().filter(|(_, d)| d.mode == "served").map(|(name, _)| name).collect()
    }

    /// Stacks referencing a config, so removing one in use is refused rather
    /// than quietly breaking a container on its next restart.
    fn config_in_use(&self, name: &str) -> Option<String> {
        self.stacks
            .values()
            .find(|s| s.configs.iter().any(|c| c == name))
            .map(|s| format!("stack {}", s.name))
    }

    fn secret_in_use(&self, name: &str) -> Option<String> {
        self.stacks
            .values()
            .find(|s| s.secrets.iter().any(|c| c == name))
            .map(|s| format!("stack {}", s.name))
    }

    /// Whether `drive` is declared in the replicated cluster config at all
    /// (served or replicated) - same minimal-shape parse as `served_drives`,
    /// for the same reason (`cl-raft` does not depend on `cl-config`).
    fn drive_known(&self, drive: &str) -> bool {
        #[derive(Deserialize)]
        struct Shape {
            #[serde(default)]
            drives: BTreeMap<String, serde_yaml::Value>,
        }
        let Some(entry) = &self.cluster_config else {
            return false;
        };
        let Ok(shape) = serde_yaml::from_str::<Shape>(&entry.yaml) else {
            return false;
        };
        shape.drives.contains_key(drive)
    }

    /// Backup targets referencing a destination, so removing one in use is
    /// refused rather than quietly orphaning its targets.
    fn destination_in_use(&self, id: &str) -> Option<String> {
        self.restic_targets
            .values()
            .find(|t| t.destination_id == id)
            .map(|t| format!("backup target {}", t.id))
    }

    /// A copy with every secret blanked, for anything that leaves the node.
    ///
    /// The lengths stay right so the ui can still show that a secret is set
    /// without ever handling its contents.
    pub fn redacted(&self) -> ClusterState {
        let mut copy = self.clone();
        for secret in copy.secrets.values_mut() {
            secret.sealed = String::new();
        }
        for cert in copy.certificates.values_mut() {
            cert.key_pem = String::new();
        }
        copy
    }

    /// Instances a node should be running right now.
    pub fn assignments_on<'a>(&'a self, node: &'a str) -> impl Iterator<Item = &'a Assignment> {
        self.assignments.values().filter(move |a| a.node == node)
    }
}

/// Base64 helpers, kept here so the encoding used for attachments is defined in
/// exactly one place.
pub fn base64_encode(raw: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(raw)
}

pub fn base64_decode(encoded: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| e.to_string())
}

/// Records are keyed by the triple that makes them unique in a zone.
pub fn dns_key(fqdn: &str, record_type: &str, content: &str) -> String {
    format!("{fqdn}|{record_type}|{content}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_run_outcome_reads_the_pre_progress_unit_variant() {
        let legacy: BackupRunOutcome = serde_json::from_str("\"Running\"").unwrap();
        assert_eq!(legacy, BackupRunOutcome::Running { percent: None });
    }

    #[test]
    fn backup_run_outcome_round_trips_the_current_shape() {
        for outcome in [
            BackupRunOutcome::Running { percent: Some(0.42) },
            BackupRunOutcome::Succeeded { snapshot_id: "abc".into(), bytes: 7 },
            BackupRunOutcome::Failed { error: "boom".into() },
        ] {
            let json = serde_json::to_string(&outcome).unwrap();
            let back: BackupRunOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(back, outcome);
        }
    }

    #[test]
    fn an_unrecognized_request_variant_deserializes_to_unknown_instead_of_erroring() {
        let from_a_newer_node = serde_json::json!({
            "SomeFutureVariant": { "field": "value" }
        });
        let req: Request = serde_json::from_value(from_a_newer_node).unwrap();
        assert_eq!(req, Request::Unknown);

        let mut state = ClusterState::default();
        let response = state.apply(&req);
        assert!(response.is_ok(), "{:?}", response.error);
    }

    #[test]
    fn every_current_request_variant_round_trips_and_is_not_unknown() {
        // One instance per real `Request` variant (excluding `Unknown`
        // itself, which has no legitimate wire shape). Guards against
        // `request_mirror::RequestMirror` drifting out of sync with
        // `Request`: a variant missing from the mirror would silently
        // deserialize as `Unknown` here instead of round-tripping.
        let requests = vec![
            Request::PutClusterConfig { yaml: "y".into(), expect_generation: 0 },
            Request::UpsertNode(node("n1", &["fast"])),
            Request::SetAvailability { node: "n1".into(), availability: Availability::Drain },
            Request::SetReachable { node: "n1".into(), reachable: false },
            Request::RemoveNode { node: "n1".into() },
            Request::RetireNodeId { id: uuid::Uuid::new_v4() },
            Request::DeployStack { name: "s".into(), spec: "spec".into() },
            Request::RemoveStack { name: "s".into() },
            Request::SetStackObjects {
                name: "s".into(),
                configs: drives(&["c1"]),
                secrets: drives(&["sec1"]),
                used_configs: drives(&["c1"]),
                used_secrets: drives(&["sec1"]),
            },
            Request::Assign { instance: InstanceId::new("s", "web", 0), node: "n1".into(), drives: drives(&["fast"]) },
            Request::Unassign { instance: InstanceId::new("s", "web", 0) },
            Request::SetDesiredState { instance: InstanceId::new("s", "web", 0), desired: DesiredState::Stopped },
            Request::SetImageDigest { instance: InstanceId::new("s", "web", 0), digest: "sha256:abc".into() },
            Request::BeginMigration { instance: InstanceId::new("s", "web", 0), to: "n2".into(), drives: drives(&["fast"]) },
            Request::AdvanceMigration { instance: InstanceId::new("s", "web", 0), phase: MigrationPhase::Syncing },
            Request::UpsertVolume { id: VolumeId::new("fast", "v1"), primary: "n1".into() },
            Request::SetVolumeReplica { id: VolumeId::new("fast", "v1"), node: "n1".into(), generation: 2 },
            Request::TouchVolume { id: VolumeId::new("fast", "v1"), primary: "n1".into() },
            Request::StoreCertificate(CertificateState {
                fqdn: "example.com".into(),
                chain_pem: "chain".into(),
                key_pem: "key".into(),
                not_after: 0,
            }),
            Request::RemoveCertificate { fqdn: "example.com".into() },
            Request::SetCertStatus { fqdn: "example.com".into(), status: ReconcileStatus::Pending },
            Request::UpsertDnsRecord(DnsRecordState {
                fqdn: "example.com".into(),
                record_type: "A".into(),
                content: "1.2.3.4".into(),
                zone: "example.com".into(),
                provider_id: None,
            }),
            Request::RemoveDnsRecord { key: "example.com".into() },
            Request::SetDnsStatus { fqdn: "example.com".into(), status: ReconcileStatus::Pending },
            Request::PutConfig { name: "c1".into(), content: "aGk=".into(), owner: None },
            Request::RemoveConfig { name: "c1".into() },
            Request::PutSecret {
                name: "sec1".into(),
                sealed: "sealed".into(),
                key_fingerprint: "fp".into(),
                owner: None,
            },
            Request::RemoveSecret { name: "sec1".into() },
            Request::PutWrappedKey {
                node: "n1".into(),
                entry: WrappedKeyEntry {
                    ephemeral_public: "e".into(),
                    sealed: "s".into(),
                    key_fingerprint: "fp".into(),
                    target_pubkey: "t".into(),
                },
            },
            Request::SetOverlayAllocations { allocations: "{}".into() },
            Request::PutBackupDestination(BackupDestination {
                id: "d1".into(),
                name: "d1".into(),
                kind: BackupDestinationKind::NodeLocal { node: "n1".into(), path: "/backups".into() },
                password_secret: "sec1".into(),
                credentials_secret: None,
            }),
            Request::RemoveBackupDestination { id: "d1".into() },
            Request::PutBackupTarget(DriveBackupTarget {
                id: "t1".into(),
                drive: "fast".into(),
                destination_id: "d1".into(),
                enabled: true,
                frequency_secs: 3600,
                time_of_day: None,
                keep_last: None,
                keep_within_secs: None,
                stop_containers: None,
                last_run: None,
            }),
            Request::RemoveBackupTarget { id: "t1".into() },
            Request::SetBackupTargetLastRun { id: "t1".into(), at: 0 },
            Request::RecordBackupRun(BackupRun {
                id: "r1".into(),
                target_id: "t1".into(),
                drive: "fast".into(),
                destination_id: "d1".into(),
                started_at: 0,
                finished_at: None,
                outcome: BackupRunOutcome::Running { percent: None },
                triggered_by: BackupTrigger::Manual { by: "op".into() },
            }),
            Request::PutRestoreRequest(RestoreRequest {
                id: "res1".into(),
                scope: RestoreScope::Volume { volume: VolumeId::new("fast", "v1") },
                destination_id: None,
                snapshot: None,
                requested_at: 0,
                finished_at: None,
                status: RestoreStatus::Pending,
                requested_by: "op".into(),
            }),
            Request::SetRestoreStatus { id: "res1".into(), status: RestoreStatus::Pending, at: 0 },
            Request::NoOp,
            Request::ClearEmergency,
        ];

        for req in requests {
            let json = serde_json::to_string(&req).unwrap();
            let back: Request = serde_json::from_str(&json).unwrap();
            assert_ne!(back, Request::Unknown, "variant round-tripped as Unknown: {json}");
            assert_eq!(back, req);
        }
    }

    #[test]
    fn cluster_state_ignores_an_unknown_top_level_field() {
        // Guards against ever adding `#[serde(deny_unknown_fields)]` here: a
        // brand-new field a newer node's `ClusterState` has must stay silently
        // ignorable by an older node reading the same JSON, with no per-field
        // work required - see the migration plan's forward-tolerance design.
        let mut value = serde_json::to_value(ClusterState::default()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("some_field_from_the_future".into(), serde_json::json!({"anything": true}));

        let parsed: Result<ClusterState, _> = serde_json::from_value(value);
        assert!(parsed.is_ok(), "{:?}", parsed.err());
    }

    #[test]
    fn the_first_cluster_config_is_stored_at_generation_one() {
        let mut state = ClusterState::default();
        let response = state.apply(&Request::PutClusterConfig {
            yaml: "cluster:\n  name: home\n".into(),
            expect_generation: 0,
        });

        assert!(response.is_ok(), "{:?}", response.error);
        let stored = state.cluster_config.expect("stored");
        assert_eq!(stored.generation, 1);
        assert!(stored.yaml.contains("home"));
    }

    #[test]
    fn a_config_edit_made_against_a_stale_generation_is_refused() {
        let mut state = ClusterState::default();
        state.apply(&Request::PutClusterConfig {
            yaml: "first".into(),
            expect_generation: 0,
        });

        // Two admins both read generation 1 and both save. The second one has
        // not seen the first one's change, so it must not land.
        let first = state.apply(&Request::PutClusterConfig {
            yaml: "second".into(),
            expect_generation: 1,
        });
        let second = state.apply(&Request::PutClusterConfig {
            yaml: "third".into(),
            expect_generation: 1,
        });

        assert!(first.is_ok());
        assert!(second.error.is_some());
        let stored = state.cluster_config.expect("stored");
        assert_eq!(stored.yaml, "second");
        assert_eq!(stored.generation, 2);
    }

    fn node(name: &str, drives: &[&str]) -> NodeState {
        NodeState {
            name: name.to_string(),
            id: uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, name.as_bytes()),
            role: Role::Manager,
            rpc_addr: "10.10.0.1:7373".parse().unwrap(),
            drives: drives
                .iter()
                .map(|d| (d.to_string(), PathBuf::from(format!("/srv/{d}"))))
                .collect(),
            availability: Availability::Active,
            labels: Default::default(),
            egress_networks: Default::default(),
            networks: Default::default(),
            open_ports: Default::default(),
            arch: Default::default(),
            binary_hash: Default::default(),
            node_pubkey: Default::default(),
        }
    }

    fn cluster() -> ClusterState {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(node("n1", &["fast", "bulk"])));
        state.apply(&Request::UpsertNode(node("n2", &["fast"])));
        state.apply(&Request::UpsertNode(node("n3", &[])));
        state
    }

    fn drives(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn assign(state: &mut ClusterState, node: &str, needs: &[&str]) -> Response {
        state.apply(&Request::Assign {
            instance: InstanceId::new("gitlab", "web", 0),
            node: node.to_string(),
            drives: drives(needs),
        })
    }

    #[test]
    fn version_advances_only_on_accepted_commands() {
        let mut state = cluster();
        let before = state.version;
        let rejected = assign(&mut state, "nowhere", &[]);
        assert!(!rejected.is_ok());
        assert_eq!(state.version, before);

        let accepted = assign(&mut state, "n1", &[]);
        assert!(accepted.is_ok());
        assert_eq!(state.version, before + 1);
    }

    #[test]
    fn reassigning_an_instance_to_a_new_node_keeps_its_image_pin() {
        let mut state = cluster();
        assert!(assign(&mut state, "n1", &[]).is_ok());

        let instance = InstanceId::new("gitlab", "web", 0);
        state.apply(&Request::SetImageDigest {
            instance: instance.clone(),
            digest: "sha256:pinned".to_string(),
        });
        assert_eq!(state.assignments[&instance].image_digest.as_deref(), Some("sha256:pinned"));

        // The scheduler moves the instance to a different node (its old one
        // went unreachable, say) by submitting `Assign` again, with no
        // intervening `Unassign`. The pin must survive that move.
        assert!(assign(&mut state, "n2", &[]).is_ok());
        assert_eq!(state.assignments[&instance].node, "n2");
        assert_eq!(state.assignments[&instance].image_digest.as_deref(), Some("sha256:pinned"));
    }

    #[test]
    fn placement_requires_every_drive() {
        let mut state = cluster();
        assert!(assign(&mut state, "n1", &["fast", "bulk"]).is_ok());

        let mut state = cluster();
        let rejected = assign(&mut state, "n2", &["fast", "bulk"]);
        assert!(rejected
            .error
            .unwrap()
            .contains("n2 does not implement drive bulk"));
    }

    /// A served drive has one authoritative copy any node can reach over the
    /// internal network, so `check_placement` must not reject a node just
    /// because it never declared a local directory for it — this is the
    /// same carve-out `cl_sched::place::rejection` applies when picking a
    /// candidate; the two have to agree, or the scheduler's own choice would
    /// always be rejected here (this state machine's independent check).
    #[test]
    fn a_served_drive_does_not_require_the_node_to_implement_it() {
        let served_config = Request::PutClusterConfig {
            yaml: "drives:\n  bulk:\n    mode: served\n    served_by: [n1]\n".into(),
            expect_generation: 0,
        };

        // n3 implements neither "fast" nor "bulk" (see `cluster()`), but
        // "bulk" is now served, so only "fast" should still block it.
        let mut state = cluster();
        state.apply(&served_config);
        assert!(assign(&mut state, "n3", &["bulk"]).is_ok());

        let mut state = cluster();
        state.apply(&served_config);
        let rejected = assign(&mut state, "n3", &["fast"]);
        assert!(rejected
            .error
            .unwrap()
            .contains("n3 does not implement drive fast"));
    }

    #[test]
    fn drained_nodes_take_no_new_work() {
        let mut state = cluster();
        state.apply(&Request::SetAvailability {
            node: "n1".into(),
            availability: Availability::Drain,
        });
        let rejected = assign(&mut state, "n1", &[]);
        assert!(rejected.error.unwrap().contains("Drain"));
    }

    #[test]
    fn an_unreachable_node_takes_no_new_work() {
        let mut state = cluster();
        state.apply(&Request::SetReachable {
            node: "n1".into(),
            reachable: false,
        });
        assert!(!state.can_schedule("n1"));

        let rejected = assign(&mut state, "n1", &[]);
        assert!(rejected.error.unwrap().contains("n1 is unreachable"));

        // The node coming back makes it a candidate again.
        state.apply(&Request::SetReachable {
            node: "n1".into(),
            reachable: true,
        });
        assert!(state.can_schedule("n1"));
        assert!(assign(&mut state, "n1", &[]).is_ok());
    }

    #[test]
    fn reachability_is_only_tracked_for_known_nodes() {
        let mut state = cluster();
        let rejected = state.apply(&Request::SetReachable {
            node: "ghost".into(),
            reachable: false,
        });
        assert!(rejected.error.unwrap().contains("unknown node ghost"));
    }

    #[test]
    fn removing_a_node_clears_its_unreachable_flag() {
        let mut state = cluster();
        state.apply(&Request::SetReachable {
            node: "n1".into(),
            reachable: false,
        });
        state.apply(&Request::RemoveNode { node: "n1".into() });
        assert!(state.unreachable.is_empty());
    }

    fn wrapped_key_entry(target_pubkey: &str) -> WrappedKeyEntry {
        WrappedKeyEntry {
            ephemeral_public: "eph".into(),
            sealed: "blob".into(),
            key_fingerprint: "fp".into(),
            target_pubkey: target_pubkey.into(),
        }
    }

    #[test]
    fn put_wrapped_key_requires_a_known_node() {
        let mut state = cluster();
        let rejected = state.apply(&Request::PutWrappedKey {
            node: "nowhere".into(),
            entry: wrapped_key_entry("pub"),
        });
        assert!(rejected.error.unwrap().contains("unknown node nowhere"));
    }

    #[test]
    fn put_wrapped_key_requires_a_matching_published_pubkey() {
        let mut state = cluster();
        // n1 has not published a pubkey yet.
        let rejected = state.apply(&Request::PutWrappedKey {
            node: "n1".into(),
            entry: wrapped_key_entry("pub"),
        });
        assert!(rejected.error.is_some());
        assert!(state.wrapped_keys.is_empty());
    }

    #[test]
    fn put_wrapped_key_accepts_and_overwrites() {
        let mut state = cluster();
        let mut n1 = state.nodes.get("n1").unwrap().clone();
        n1.node_pubkey = Some("pub-a".into());
        state.apply(&Request::UpsertNode(n1));

        assert!(state
            .apply(&Request::PutWrappedKey {
                node: "n1".into(),
                entry: wrapped_key_entry("pub-a"),
            })
            .is_ok());
        assert_eq!(state.wrapped_keys["n1"].target_pubkey, "pub-a");

        // A rotated keypair: the old wrapped entry no longer matches.
        let mut n1 = state.nodes.get("n1").unwrap().clone();
        n1.node_pubkey = Some("pub-b".into());
        state.apply(&Request::UpsertNode(n1));
        let rejected = state.apply(&Request::PutWrappedKey {
            node: "n1".into(),
            entry: wrapped_key_entry("pub-a"),
        });
        assert!(rejected.error.is_some());

        assert!(state
            .apply(&Request::PutWrappedKey {
                node: "n1".into(),
                entry: wrapped_key_entry("pub-b"),
            })
            .is_ok());
        assert_eq!(state.wrapped_keys["n1"].target_pubkey, "pub-b");
    }

    #[test]
    fn removing_a_node_clears_its_wrapped_key() {
        let mut state = cluster();
        let mut n1 = state.nodes.get("n1").unwrap().clone();
        n1.node_pubkey = Some("pub-a".into());
        state.apply(&Request::UpsertNode(n1));
        state.apply(&Request::PutWrappedKey {
            node: "n1".into(),
            entry: wrapped_key_entry("pub-a"),
        });
        assert!(!state.wrapped_keys.is_empty());

        state.apply(&Request::RemoveNode { node: "n1".into() });
        assert!(!state.wrapped_keys.contains_key("n1"));
    }

    #[test]
    fn nodes_running_work_cannot_be_removed() {
        let mut state = cluster();
        assign(&mut state, "n1", &[]);
        let rejected = state.apply(&Request::RemoveNode { node: "n1".into() });
        assert!(rejected.error.unwrap().contains("still runs gitlab.web.0"));

        state.apply(&Request::Unassign {
            instance: InstanceId::new("gitlab", "web", 0),
        });
        assert!(state.apply(&Request::RemoveNode { node: "n1".into() }).is_ok());
        assert!(!state.nodes.contains_key("n1"));
    }

    #[test]
    fn redeploying_a_stack_bumps_the_revision() {
        let mut state = ClusterState::default();
        state.apply(&Request::DeployStack {
            name: "gitlab".into(),
            spec: "a".into(),
        });
        assert_eq!(state.stacks["gitlab"].revision, 1);
        state.apply(&Request::DeployStack {
            name: "gitlab".into(),
            spec: "b".into(),
        });
        assert_eq!(state.stacks["gitlab"].revision, 2);
        assert_eq!(state.stacks["gitlab"].spec, "b");
    }

    #[test]
    fn removing_a_stack_drops_its_assignments() {
        let mut state = cluster();
        assign(&mut state, "n1", &[]);
        state.apply(&Request::RemoveStack {
            name: "gitlab".into(),
        });
        assert!(state.assignments.is_empty());
    }

    #[test]
    fn migration_walks_the_phases_in_order() {
        let mut state = cluster();
        let instance = InstanceId::new("gitlab", "web", 0);
        assign(&mut state, "n1", &["fast"]);

        assert!(state
            .apply(&Request::BeginMigration {
                instance: instance.clone(),
                to: "n2".into(),
                drives: drives(&["fast"]),
            })
            .is_ok());

        // Skipping ahead is refused, so a stale message cannot rewind a move.
        let skipped = state.apply(&Request::AdvanceMigration {
            instance: instance.clone(),
            phase: MigrationPhase::Starting,
        });
        assert!(skipped.error.unwrap().contains("cannot advance"));

        for phase in [
            MigrationPhase::Syncing,
            MigrationPhase::Verifying,
            MigrationPhase::Switching,
        ] {
            assert!(state
                .apply(&Request::AdvanceMigration {
                    instance: instance.clone(),
                    phase,
                })
                .is_ok());
        }
        // Ownership changes at the switch, before the container starts.
        assert_eq!(state.assignments[&instance].node, "n2");

        state.apply(&Request::AdvanceMigration {
            instance: instance.clone(),
            phase: MigrationPhase::Starting,
        });
        state.apply(&Request::AdvanceMigration {
            instance: instance.clone(),
            phase: MigrationPhase::Done,
        });
        assert!(state.assignments[&instance].migration.is_none());
    }

    #[test]
    fn re_recording_the_current_phase_is_a_no_op() {
        let mut state = cluster();
        let instance = InstanceId::new("gitlab", "web", 0);
        assign(&mut state, "n1", &["fast"]);

        state.apply(&Request::BeginMigration {
            instance: instance.clone(),
            to: "n2".into(),
            drives: drives(&["fast"]),
        });
        state.apply(&Request::AdvanceMigration {
            instance: instance.clone(),
            phase: MigrationPhase::Syncing,
        });

        // A driver that retried a submit it never saw the answer to must be
        // told "already done", not "not allowed": the second reads as a
        // failure and would abandon a move that is in fact on track.
        let repeat = state.apply(&Request::AdvanceMigration {
            instance: instance.clone(),
            phase: MigrationPhase::Syncing,
        });
        assert!(repeat.error.is_none());
        assert_eq!(
            state.assignments[&instance].migration.as_ref().unwrap().phase,
            MigrationPhase::Syncing
        );

        // Going backwards is still refused.
        let rewind = state.apply(&Request::AdvanceMigration {
            instance: instance.clone(),
            phase: MigrationPhase::Stopping,
        });
        assert!(rewind.error.unwrap().contains("cannot advance"));
    }

    #[test]
    fn a_migration_records_the_node_driving_it() {
        let mut state = cluster();
        let instance = InstanceId::new("gitlab", "web", 0);
        assign(&mut state, "n1", &["fast"]);

        state.apply(&Request::BeginMigration {
            instance: instance.clone(),
            to: "n2".into(),
            drives: drives(&["fast"]),
        });
        let migration = state.assignments[&instance].migration.clone().unwrap();
        assert_eq!(migration.from, "n1");

        // `Switching` repoints the assignment, so `from` is the only thing
        // that still says whose job this is.
        for phase in [
            MigrationPhase::Syncing,
            MigrationPhase::Verifying,
            MigrationPhase::Switching,
        ] {
            state.apply(&Request::AdvanceMigration {
                instance: instance.clone(),
                phase,
            });
        }
        assert_eq!(state.assignments[&instance].node, "n2");
        assert_eq!(
            state.assignments[&instance].migration.as_ref().unwrap().from,
            "n1"
        );
    }

    #[test]
    fn migration_target_must_implement_the_drives() {
        let mut state = cluster();
        let instance = InstanceId::new("gitlab", "web", 0);
        assign(&mut state, "n1", &["bulk"]);
        let rejected = state.apply(&Request::BeginMigration {
            instance,
            to: "n2".into(),
            drives: drives(&["bulk"]),
        });
        assert!(rejected.error.unwrap().contains("does not implement drive bulk"));
    }

    #[test]
    fn migration_can_fail_from_any_phase() {
        let mut state = cluster();
        let instance = InstanceId::new("gitlab", "web", 0);
        assign(&mut state, "n1", &["fast"]);
        state.apply(&Request::BeginMigration {
            instance: instance.clone(),
            to: "n2".into(),
            drives: drives(&["fast"]),
        });
        assert!(state
            .apply(&Request::AdvanceMigration {
                instance: instance.clone(),
                phase: MigrationPhase::Failed {
                    reason: "sync timed out".into()
                },
            })
            .is_ok());
        // The instance stays on the source node when the move fails early.
        assert_eq!(state.assignments[&instance].node, "n1");
    }

    #[test]
    fn one_migration_at_a_time() {
        let mut state = cluster();
        let instance = InstanceId::new("gitlab", "web", 0);
        assign(&mut state, "n1", &["fast"]);
        state.apply(&Request::BeginMigration {
            instance: instance.clone(),
            to: "n2".into(),
            drives: drives(&["fast"]),
        });
        let rejected = state.apply(&Request::BeginMigration {
            instance,
            to: "n3".into(),
            drives: drives(&[]),
        });
        assert!(rejected.error.unwrap().contains("already migrating"));
    }

    #[test]
    fn volume_generation_marks_replicas_stale() {
        let mut state = cluster();
        let id = VolumeId::new("fast", "data");
        state.apply(&Request::UpsertVolume {
            id: id.clone(),
            primary: "n1".into(),
        });
        state.apply(&Request::SetVolumeReplica {
            id: id.clone(),
            node: "n2".into(),
            generation: 1,
        });
        assert_eq!(state.volumes[&id].in_sync(), ["n1", "n2"].into());

        state.apply(&Request::TouchVolume {
            id: id.clone(),
            primary: "n1".into(),
        });
        assert_eq!(state.volumes[&id].in_sync(), ["n1"].into());
        assert_eq!(state.volumes[&id].generation, 2);
    }

    #[test]
    fn replica_progress_never_goes_backwards() {
        let mut state = cluster();
        let id = VolumeId::new("fast", "data");
        state.apply(&Request::UpsertVolume {
            id: id.clone(),
            primary: "n1".into(),
        });
        state.apply(&Request::TouchVolume {
            id: id.clone(),
            primary: "n1".into(),
        });
        state.apply(&Request::SetVolumeReplica {
            id: id.clone(),
            node: "n2".into(),
            generation: 2,
        });
        // A late report from an older sync must not undo the newer one.
        state.apply(&Request::SetVolumeReplica {
            id: id.clone(),
            node: "n2".into(),
            generation: 1,
        });
        assert_eq!(state.volumes[&id].replicas["n2"], 2);
    }

    #[test]
    fn volumes_only_live_on_implementing_nodes() {
        let mut state = cluster();
        let rejected = state.apply(&Request::UpsertVolume {
            id: VolumeId::new("bulk", "data"),
            primary: "n2".into(),
        });
        assert!(rejected.error.unwrap().contains("does not implement drive bulk"));
    }

    #[test]
    fn assignments_on_filters_by_node() {
        let mut state = cluster();
        assign(&mut state, "n1", &[]);
        state.apply(&Request::Assign {
            instance: InstanceId::new("gitlab", "web", 1),
            node: "n2".into(),
            drives: drives(&["fast"]),
        });
        assert_eq!(state.assignments_on("n1").count(), 1);
        assert_eq!(state.assignments_on("n2").count(), 1);
        assert_eq!(state.assignments_on("n3").count(), 0);
    }

    #[test]
    fn state_round_trips_through_json() {
        let mut state = cluster();
        assign(&mut state, "n1", &["fast"]);
        let encoded = serde_json::to_vec(&state).unwrap();
        let decoded: ClusterState = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(state, decoded);
    }

    #[test]
    fn configs_are_stored_and_versioned() {
        let mut state = ClusterState::default();
        assert!(state
            .apply(&Request::PutConfig {
                name: "nginx_conf".into(),
                content: base64_encode(b"server {}"),
                owner: None,
            })
            .is_ok());
        assert_eq!(state.configs["nginx_conf"].revision, 1);
        assert_eq!(state.configs["nginx_conf"].decode().unwrap(), b"server {}");

        state.apply(&Request::PutConfig {
            name: "nginx_conf".into(),
            content: base64_encode(b"server { listen 80; }"),
            owner: None,
        });
        assert_eq!(state.configs["nginx_conf"].revision, 2);
    }

    #[test]
    fn secrets_are_stored_sealed_and_redacted() {
        let mut state = ClusterState::default();
        state.apply(&Request::PutSecret {
            name: "db_password".into(),
            sealed: base64_encode(b"ciphertext-goes-here"),
            key_fingerprint: "abc123".into(),
            owner: None,
        });

        assert_eq!(state.secrets["db_password"].key_fingerprint, "abc123");
        // Nothing about the value leaves the node.
        assert!(state.redacted().secrets["db_password"].sealed.is_empty());
        // Nor reaches a log line.
        let rendered = format!("{:?}", state.secrets["db_password"]);
        assert!(!rendered.contains("ciphertext"));
    }

    /// The self-update deploy key rides the ordinary secrets map under a
    /// reserved name, with no special casing in `redacted()`. This confirms
    /// that falls out for free rather than needing its own blanking rule.
    #[test]
    fn redacted_blanks_the_self_update_deploy_key_like_any_other_secret() {
        let mut state = ClusterState::default();
        state.apply(&Request::PutSecret {
            name: "_clouduler_self_update_deploy_key".into(),
            sealed: base64_encode(b"ssh-private-key-ciphertext"),
            key_fingerprint: "abc123".into(),
            owner: None,
        });
        assert!(state
            .redacted()
            .secrets["_clouduler_self_update_deploy_key"]
            .sealed
            .is_empty());
    }

    #[test]
    fn object_names_must_be_safe_as_filenames() {
        let mut state = ClusterState::default();
        for bad in ["../escape", "with/slash", ".hidden", ""] {
            let rejected = state.apply(&Request::PutConfig {
                name: bad.into(),
                content: base64_encode(b"x"),
                owner: None,
            });
            assert!(!rejected.is_ok(), "{bad} should be refused");
        }
        assert!(state.configs.is_empty());
    }

    #[test]
    fn objects_in_use_cannot_be_removed() {
        let mut state = ClusterState::default();
        state.apply(&Request::DeployStack {
            name: "site".into(),
            spec: "services: {}".into(),
        });
        state.apply(&Request::PutConfig {
            name: "nginx_conf".into(),
            content: base64_encode(b"x"),
            owner: Some("site".into()),
        });
        state.apply(&Request::SetStackObjects {
            name: "site".into(),
            configs: BTreeSet::from(["nginx_conf".to_string()]),
            secrets: BTreeSet::new(),
            used_configs: BTreeSet::new(),
            used_secrets: BTreeSet::new(),
        });

        let rejected = state.apply(&Request::RemoveConfig {
            name: "nginx_conf".into(),
        });
        assert!(rejected.error.unwrap().contains("still used by stack site"));
    }

    #[test]
    fn removing_a_stack_takes_its_objects_with_it() {
        let mut state = ClusterState::default();
        state.apply(&Request::DeployStack {
            name: "site".into(),
            spec: "services: {}".into(),
        });
        state.apply(&Request::PutConfig {
            name: "nginx_conf".into(),
            content: base64_encode(b"x"),
            owner: Some("site".into()),
        });
        state.apply(&Request::PutSecret {
            name: "db_password".into(),
            sealed: base64_encode(b"y"),
            key_fingerprint: "abc".into(),
            owner: Some("site".into()),
        });
        state.apply(&Request::SetStackObjects {
            name: "site".into(),
            configs: BTreeSet::from(["nginx_conf".to_string()]),
            secrets: BTreeSet::from(["db_password".to_string()]),
            used_configs: BTreeSet::new(),
            used_secrets: BTreeSet::new(),
        });

        state.apply(&Request::RemoveStack {
            name: "site".into(),
        });
        assert!(state.configs.is_empty());
        assert!(state.secrets.is_empty());
    }

    #[test]
    fn redeploying_a_stack_keeps_its_object_ownership() {
        let mut state = ClusterState::default();
        state.apply(&Request::DeployStack {
            name: "site".into(),
            spec: "a".into(),
        });
        state.apply(&Request::SetStackObjects {
            name: "site".into(),
            configs: BTreeSet::from(["c".to_string()]),
            secrets: BTreeSet::new(),
            used_configs: BTreeSet::new(),
            used_secrets: BTreeSet::new(),
        });
        state.apply(&Request::DeployStack {
            name: "site".into(),
            spec: "b".into(),
        });
        assert_eq!(state.stacks["site"].configs.len(), 1);
    }

    #[test]
    fn an_unused_object_can_be_removed() {
        let mut state = ClusterState::default();
        state.apply(&Request::PutConfig {
            name: "orphan".into(),
            content: base64_encode(b"x"),
            owner: None,
        });
        assert!(state
            .apply(&Request::RemoveConfig {
                name: "orphan".into()
            })
            .is_ok());
        assert!(state.configs.is_empty());
    }

    #[test]
    fn storing_a_certificate_clears_its_pending_status() {
        let mut state = ClusterState::default();
        state.apply(&Request::SetCertStatus {
            fqdn: "app.example.com".into(),
            status: ReconcileStatus::Pending,
        });
        assert_eq!(state.cert_status.get("app.example.com"), Some(&ReconcileStatus::Pending));

        state.apply(&Request::StoreCertificate(CertificateState {
            fqdn: "app.example.com".into(),
            chain_pem: "chain".into(),
            key_pem: "key".into(),
            not_after: 0,
        }));

        assert!(state.certificates.contains_key("app.example.com"));
        assert!(!state.cert_status.contains_key("app.example.com"));
    }

    #[test]
    fn upserting_a_dns_record_clears_its_status_for_that_fqdn() {
        let mut state = ClusterState::default();
        state.apply(&Request::SetDnsStatus {
            fqdn: "app.example.com".into(),
            status: ReconcileStatus::Error {
                message: "no zone".into(),
                at: 0,
            },
        });
        assert!(state.dns_status.contains_key("app.example.com"));

        state.apply(&Request::UpsertDnsRecord(DnsRecordState {
            fqdn: "app.example.com".into(),
            record_type: "A".into(),
            content: "10.0.0.1".into(),
            zone: "example.com".into(),
            provider_id: Some("rec1".into()),
        }));

        assert!(state.dns_records.values().any(|r| r.fqdn == "app.example.com"));
        assert!(!state.dns_status.contains_key("app.example.com"));
    }

    /// A cluster with "fast" declared in the replicated config, so
    /// `drive_known` sees it - `cluster()` alone only makes nodes implement
    /// drives, it never publishes a config for `drive_known` to read.
    fn cluster_with_known_drive() -> ClusterState {
        let mut state = cluster();
        state.apply(&Request::PutClusterConfig {
            yaml: "drives:\n  fast:\n    mode: replicate\n".into(),
            expect_generation: 0,
        });
        state
    }

    fn node_local_destination(id: &str, node: &str) -> BackupDestination {
        BackupDestination {
            id: id.to_string(),
            name: id.to_string(),
            kind: BackupDestinationKind::NodeLocal {
                node: node.to_string(),
                path: "/srv/backups".into(),
            },
            password_secret: format!("_clouduler_restic_password_{id}"),
            credentials_secret: None,
        }
    }

    fn backup_target(id: &str, drive: &str, destination_id: &str, enabled: bool) -> DriveBackupTarget {
        DriveBackupTarget {
            id: id.to_string(),
            drive: drive.to_string(),
            destination_id: destination_id.to_string(),
            enabled,
            frequency_secs: 86_400,
            time_of_day: None,
            keep_last: Some(7),
            keep_within_secs: None,
            stop_containers: None,
            last_run: None,
        }
    }

    #[test]
    fn a_node_local_destination_must_name_a_known_node() {
        let mut state = cluster();
        let rejected = state.apply(&Request::PutBackupDestination(node_local_destination(
            "d1", "nowhere",
        )));
        assert!(rejected.error.unwrap().contains("unknown node nowhere"));
    }

    #[test]
    fn a_backup_target_must_name_a_known_drive_and_destination() {
        let mut state = cluster_with_known_drive();
        state.apply(&Request::PutBackupDestination(node_local_destination("d1", "n1")));

        let unknown_drive = state.apply(&Request::PutBackupTarget(backup_target(
            "t1", "nowhere", "d1", true,
        )));
        assert!(unknown_drive.error.unwrap().contains("unknown drive nowhere"));

        let unknown_destination = state.apply(&Request::PutBackupTarget(backup_target(
            "t1", "fast", "nowhere", true,
        )));
        assert!(unknown_destination
            .error
            .unwrap()
            .contains("unknown backup destination nowhere"));

        assert!(state
            .apply(&Request::PutBackupTarget(backup_target("t1", "fast", "d1", true)))
            .is_ok());
        assert!(state.restic_targets.contains_key("t1"));
    }

    #[test]
    fn two_enabled_targets_cannot_share_a_drive_and_destination() {
        let mut state = cluster_with_known_drive();
        state.apply(&Request::PutBackupDestination(node_local_destination("d1", "n1")));
        state.apply(&Request::PutBackupTarget(backup_target("t1", "fast", "d1", true)));

        let rejected = state.apply(&Request::PutBackupTarget(backup_target("t2", "fast", "d1", true)));
        assert!(rejected
            .error
            .unwrap()
            .contains("already has an enabled backup target"));

        // A disabled duplicate is fine (e.g. a draft being edited).
        assert!(state
            .apply(&Request::PutBackupTarget(backup_target("t2", "fast", "d1", false)))
            .is_ok());
    }

    #[test]
    fn removing_a_destination_in_use_by_a_target_is_refused() {
        let mut state = cluster_with_known_drive();
        state.apply(&Request::PutBackupDestination(node_local_destination("d1", "n1")));
        state.apply(&Request::PutBackupTarget(backup_target("t1", "fast", "d1", true)));

        let rejected = state.apply(&Request::RemoveBackupDestination { id: "d1".into() });
        assert!(rejected.error.unwrap().contains("still used by backup target t1"));

        state.apply(&Request::RemoveBackupTarget { id: "t1".into() });
        assert!(state
            .apply(&Request::RemoveBackupDestination { id: "d1".into() })
            .is_ok());
        assert!(!state.restic_destinations.contains_key("d1"));
    }

    #[test]
    fn setting_last_run_updates_only_the_named_target() {
        let mut state = cluster_with_known_drive();
        state.apply(&Request::PutBackupDestination(node_local_destination("d1", "n1")));
        state.apply(&Request::PutBackupTarget(backup_target("t1", "fast", "d1", true)));

        assert!(state
            .apply(&Request::SetBackupTargetLastRun { id: "t1".into(), at: 100 })
            .is_ok());
        assert_eq!(state.restic_targets["t1"].last_run, Some(100));

        let rejected = state.apply(&Request::SetBackupTargetLastRun { id: "nowhere".into(), at: 0 });
        assert!(rejected.error.unwrap().contains("unknown backup target nowhere"));
    }

    #[test]
    fn a_second_running_run_for_the_same_target_is_rejected() {
        let mut state = cluster_with_known_drive();
        let first = BackupRun {
            id: "run1".into(),
            target_id: "t1".into(),
            drive: "fast".into(),
            destination_id: "d1".into(),
            started_at: 0,
            finished_at: None,
            outcome: BackupRunOutcome::Running { percent: None },
            triggered_by: BackupTrigger::Schedule,
        };
        assert!(state.apply(&Request::RecordBackupRun(first.clone())).is_ok());

        let second = BackupRun { id: "run2".into(), ..first.clone() };
        let rejected = state.apply(&Request::RecordBackupRun(second));
        assert!(rejected.error.unwrap().contains("already running"));

        // A progress update against the same run id is not a new run and is
        // always allowed.
        let mut progressed = first.clone();
        progressed.outcome = BackupRunOutcome::Running { percent: Some(0.5) };
        assert!(state.apply(&Request::RecordBackupRun(progressed)).is_ok());

        // Finishing the first run frees the target up for a new one.
        let mut finished = first;
        finished.outcome = BackupRunOutcome::Succeeded { snapshot_id: "abc".into(), bytes: 0 };
        assert!(state.apply(&Request::RecordBackupRun(finished)).is_ok());
        let third = BackupRun {
            id: "run3".into(),
            target_id: "t1".into(),
            drive: "fast".into(),
            destination_id: "d1".into(),
            started_at: 1,
            finished_at: None,
            outcome: BackupRunOutcome::Running { percent: None },
            triggered_by: BackupTrigger::Schedule,
        };
        assert!(state.apply(&Request::RecordBackupRun(third)).is_ok());
    }

    #[test]
    fn recording_a_run_prunes_older_runs_past_the_cap_for_that_target() {
        let mut state = cluster_with_known_drive();
        for i in 0..(MAX_BACKUP_RUNS_PER_TARGET + 5) {
            state.apply(&Request::RecordBackupRun(BackupRun {
                id: format!("run{i}"),
                target_id: "t1".into(),
                drive: "fast".into(),
                destination_id: "d1".into(),
                started_at: i as i64,
                finished_at: Some(i as i64 + 1),
                outcome: BackupRunOutcome::Succeeded {
                    snapshot_id: "abc".into(),
                    bytes: 0,
                },
                triggered_by: BackupTrigger::Schedule,
            }));
        }
        let for_target: Vec<_> = state
            .restic_runs
            .values()
            .filter(|r| r.drive == "fast" && r.destination_id == "d1")
            .collect();
        assert_eq!(for_target.len(), MAX_BACKUP_RUNS_PER_TARGET);
        // The oldest runs (run0..run4) were the ones dropped.
        assert!(!state.restic_runs.contains_key("run0"));
        assert!(state.restic_runs.contains_key(&format!("run{}", MAX_BACKUP_RUNS_PER_TARGET + 4)));
    }

    fn restore_request(id: &str, drive: &str) -> RestoreRequest {
        RestoreRequest {
            id: id.to_string(),
            scope: RestoreScope::Drive { drive: drive.to_string() },
            destination_id: None,
            snapshot: None,
            requested_at: 0,
            finished_at: None,
            status: RestoreStatus::Pending,
            requested_by: "tester".into(),
        }
    }

    #[test]
    fn a_restore_request_must_name_a_known_drive() {
        let mut state = cluster();
        let rejected = state.apply(&Request::PutRestoreRequest(restore_request("r1", "nowhere")));
        assert!(rejected.error.unwrap().contains("unknown drive nowhere"));
    }

    #[test]
    fn a_restore_request_always_lands_pending_regardless_of_the_caller() {
        let mut state = cluster_with_known_drive();
        let mut request = restore_request("r1", "fast");
        request.status = RestoreStatus::Succeeded;
        request.finished_at = Some(999);
        assert!(state.apply(&Request::PutRestoreRequest(request)).is_ok());
        assert_eq!(state.restore_requests["r1"].status, RestoreStatus::Pending);
        assert_eq!(state.restore_requests["r1"].finished_at, None);
    }

    #[test]
    fn only_the_first_claim_of_a_pending_restore_succeeds() {
        let mut state = cluster_with_known_drive();
        state.apply(&Request::PutRestoreRequest(restore_request("r1", "fast")));

        assert!(state
            .apply(&Request::SetRestoreStatus { id: "r1".into(), status: RestoreStatus::Running, at: 10 })
            .is_ok());
        let rejected = state.apply(&Request::SetRestoreStatus { id: "r1".into(), status: RestoreStatus::Running, at: 11 });
        assert!(rejected.error.unwrap().contains("not pending"));
    }

    #[test]
    fn a_terminal_restore_status_cannot_be_advanced_further() {
        let mut state = cluster_with_known_drive();
        state.apply(&Request::PutRestoreRequest(restore_request("r1", "fast")));
        state.apply(&Request::SetRestoreStatus { id: "r1".into(), status: RestoreStatus::Running, at: 10 });
        assert!(state
            .apply(&Request::SetRestoreStatus {
                id: "r1".into(),
                status: RestoreStatus::Failed { error: "boom".into() },
                at: 20,
            })
            .is_ok());
        assert_eq!(state.restore_requests["r1"].finished_at, Some(20));

        let rejected = state.apply(&Request::SetRestoreStatus { id: "r1".into(), status: RestoreStatus::Running, at: 30 });
        assert!(rejected.error.unwrap().contains("already finished"));
    }
}
