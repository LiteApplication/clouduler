//! The replicated cluster state and the commands that mutate it. `apply` is the
//! only way state changes and it never does IO, keeping the state machine
//! deterministic across nodes and its invariants unit testable.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::alerts::{
    AlertRecord, AlertSignal, DestinationHealth, MAX_ALERT_INBOX, MAX_ALERT_RECORDS,
    MAX_RECORD_DELIVERIES,
};
use crate::ids::{InstanceId, VolumeId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Availability {
    Active,
    /// Keeps running what it has, takes nothing new. Set before a migration.
    Drain,
    Decommissioned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Manager,
    Worker,
}

/// A node as the cluster sees it; seeded from config, edited at runtime via the ui.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeState {
    pub name: String,
    /// Config-declared identity (`cl_config::Node::id`); see `ClusterState::removed`.
    pub id: uuid::Uuid,
    pub role: Role,
    pub rpc_addr: SocketAddr,
    pub drives: BTreeMap<String, PathBuf>,
    pub availability: Availability,
    /// Config labels, replicated for `node.labels.<key>` constraints.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// `cl_config::Node::last_resort`, replicated so the scheduler can rank this node last.
    #[serde(default)]
    pub last_resort: bool,
    /// Egress-kind networks this node has a host address for; placement requires it, like `drives`.
    #[serde(default)]
    pub egress_networks: BTreeSet<String>,
    /// Every network this node has an address on, by name; placement requires it, like `drives`.
    #[serde(default)]
    pub networks: BTreeSet<String>,
    /// Ports this node's public-network router forwards to it, by network name; a raw
    /// `x-clouduler.expose` port is only placed on a node listing its `(protocol, port)`.
    #[serde(default)]
    pub open_ports: BTreeMap<String, BTreeSet<(PortProtocol, u16)>>,
    /// `std::env::consts::ARCH` of the running binary; groups nodes by native build for self-update.
    #[serde(default)]
    pub arch: String,
    /// Blake3 hex of the running binary; confirms a restarted node runs the new build.
    #[serde(default)]
    pub binary_hash: String,
    /// X25519 public key (base64), self-reported once the local keypair exists, so a
    /// key holder can wrap the cluster secret key for this node (`wrapped_keys`).
    #[serde(default)]
    pub node_pubkey: Option<String>,
    /// Total system memory in bytes, self-reported (`report_self` in `cl-daemon`); 0
    /// until reported. The scheduler weighs memory pressure with it.
    #[serde(default)]
    pub memory_total_bytes: u64,
    /// Release version of the running binary, self-reported; empty until then. Display only.
    #[serde(default)]
    pub version: String,
    /// GPUs and device nodes found on the node, self-reported; empty until reported
    /// (read as "no GPUs"). Boxed to keep `Request::UpsertNode` small.
    #[serde(default)]
    pub gpus: Box<NodeGpus>,
    /// Image references in this node's docker (`repo:tag`, `repo@sha256:...`),
    /// self-reported; lets the scheduler prefer a node holding a pinned image when
    /// a registry is unreachable.
    #[serde(default)]
    pub images: BTreeSet<String>,
}

impl NodeState {
    /// Whether this node holds `image`, matched on `digest` when given.
    pub fn holds_image(&self, image: &str, digest: Option<&str>) -> bool {
        match digest {
            Some(d) => {
                let suffix = format!("@{d}");
                self.images.iter().any(|r| r.ends_with(&suffix))
            }
            None => self.images.contains(image),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvidiaGpu {
    pub index: u32,
    pub uuid: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderNode {
    pub path: String,
    #[serde(default)]
    pub driver: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeGpus {
    #[serde(default)]
    pub nvidia_devices: Vec<String>,
    /// GPUs listed by `nvidia-smi`; may be empty on a node with device nodes but no `nvidia-smi`.
    #[serde(default)]
    pub nvidia: Vec<NvidiaGpu>,
    /// Whether docker has an `nvidia` runtime registered.
    #[serde(default)]
    pub nvidia_runtime: bool,
    /// Path of `nvidia-container-runtime-hook`/`nvidia-container-cli` if found; docker
    /// device requests also work with only this and no registered runtime.
    #[serde(default)]
    pub nvidia_hook: Option<String>,
    #[serde(default)]
    pub render_nodes: Vec<RenderNode>,
    /// Host device paths a service's `devices:` entry may name: entries under `/dev`
    /// and a few device directories (`dri`, `snd`, `input`, `dvb`, `nvidia-caps`), and those directories.
    #[serde(default)]
    pub dev_paths: BTreeSet<String>,
}

impl NodeGpus {
    /// How many NVIDIA GPUs docker could hand out: the larger of `nvidia-smi` and device-node counts.
    pub fn nvidia_count(&self) -> usize {
        self.nvidia.len().max(self.nvidia_devices.len())
    }

    /// Whether `id` (GPU index or uuid) names a GPU this node has.
    pub fn has_nvidia_id(&self, id: &str) -> bool {
        if self
            .nvidia
            .iter()
            .any(|g| g.uuid == id || g.index.to_string() == id)
        {
            return true;
        }
        // No nvidia-smi: only an index can be checked.
        self.nvidia.is_empty()
            && id
                .parse::<usize>()
                .is_ok_and(|i| self.nvidia_devices.contains(&format!("/dev/nvidia{i}")))
    }

    /// Whether docker can serve an NVIDIA request: runtime registered or hook/cli installed.
    pub fn nvidia_ready(&self) -> bool {
        self.nvidia_runtime || self.nvidia_hook.is_some()
    }

    pub fn has_device(&self, path: &str) -> bool {
        self.dev_paths.contains(path.trim_end_matches('/'))
    }
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

    pub fn schedulable(&self) -> bool {
        self.availability == Availability::Active
    }
}

/// Transport protocol for a router-forwarded port; mirrors `cl_config::stack::Protocol`
/// so `cl-raft` does not depend on `cl-config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortProtocol {
    Tcp,
    Udp,
}

/// A deployed stack; the spec is the raw submitted yaml so the ui can round trip it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StackState {
    pub name: String,
    pub spec: String,
    /// Bumped on every redeploy, to tell a spec change from a restart.
    pub revision: u64,
    /// Cluster configs this stack brought with it (removed with it): those it bundled a `file:` for.
    #[serde(default)]
    pub configs: BTreeSet<String>,
    /// Cluster secrets this stack brought with it; same ownership as `configs`.
    #[serde(default)]
    pub secrets: BTreeSet<String>,
    /// Every cluster config any service references, bundled or not, so the ui need not reparse the spec.
    #[serde(default)]
    pub used_configs: BTreeSet<String>,
    /// Every cluster secret any service references; relates to `secrets` as `used_configs` to `configs`.
    #[serde(default)]
    pub used_secrets: BTreeSet<String>,
    /// Unix seconds of the latest `DeployStack`, from the request so `apply` stays clock-free; 0 if predating this.
    #[serde(default)]
    pub deployed_at: i64,
    /// The spec before the latest deploy that changed it (for "changes since last deploy"); `None` on a first deploy.
    #[serde(default)]
    pub previous_spec: Option<String>,
}

/// The cluster config file, replicated so an edit on one node reaches every
/// node's file (a node down during the edit would otherwise return stale). Raw
/// yaml so `cl-raft` does not depend on `cl-config` and comments/key order survive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterConfigEntry {
    pub yaml: String,
    /// Bumped on every accepted edit; a node writes the file only when this moves.
    pub generation: u64,
}

/// A cluster wide config object, mounted read only into referencing containers. Stored in the clear.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigEntry {
    pub name: String,
    pub content: String,
    /// Bumped on every update, so nodes can tell their mounted copy is stale.
    pub revision: u64,
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

/// A cluster wide secret mounted at `/run/secrets/<name>`. Sealed before it
/// reaches here, so the database, snapshots and backups hold only ciphertext; a
/// node without the cluster key replicates it but cannot run containers needing it.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretEntry {
    pub name: String,
    pub sealed: String,
    pub revision: u64,
    /// Fingerprint of the sealing key, so a node with the wrong key can say so.
    pub key_fingerprint: String,
    #[serde(default)]
    pub owner: Option<String>,
}

/// Prints no ciphertext: nothing about a secret belongs in a log line.
impl fmt::Debug for SecretEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretEntry")
            .field("name", &self.name)
            .field("revision", &self.revision)
            .field("key_fingerprint", &self.key_fingerprint)
            .finish_non_exhaustive()
    }
}

/// The cluster secret key encrypted to one node's public key; the plaintext never appears in this state.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct WrappedKeyEntry {
    pub ephemeral_public: String,
    pub sealed: String,
    pub key_fingerprint: String,
    /// The recipient public key; compared with the node's current `node_pubkey` to notice a rotation and re-wrap.
    pub target_pubkey: String,
}

/// Prints no ciphertext (see `SecretEntry`).
impl fmt::Debug for WrappedKeyEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WrappedKeyEntry")
            .field("key_fingerprint", &self.key_fingerprint)
            .field("target_pubkey", &self.target_pubkey)
            .finish_non_exhaustive()
    }
}

/// Cap on one config or secret: it goes through the raft log and every snapshot.
pub const MAX_OBJECT_BYTES: usize = 1024 * 1024;

/// Names become filenames under the mount directory, so they must be safe to join onto a path.
pub(crate) fn validate_object_name(name: &str) -> Result<(), String> {
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    pub instance: InstanceId,
    pub node: String,
    pub desired: DesiredState,
    /// Pinned image digest, set once pulled so all replicas run the same bits even for `:latest`.
    pub image_digest: Option<String>,
    pub migration: Option<Migration>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Migration {
    /// The node driving the move (where the instance started). Recorded because
    /// `Switching` repoints the assignment at the target, after which a node that
    /// crashed mid-move could not otherwise recognise its own job.
    pub from: String,
    pub to: String,
    pub phase: MigrationPhase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MigrationPhase {
    Stopping,
    Syncing,
    Verifying,
    Switching,
    Starting,
    Done,
    Failed { reason: String },
}

impl MigrationPhase {
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeState {
    pub id: VolumeId,
    /// Bumped by whichever node last wrote the volume; shows if a copy is behind without hashing.
    pub generation: u64,
    pub primary: String,
    pub replicas: BTreeMap<String, u64>,
    /// Set while an operator hand-edits the volume: replication is paused so nothing syncs over the edits.
    #[serde(default)]
    pub frozen: Option<FrozenVolume>,
    /// Set while the source refuses to replicate because pushing would wipe data off a
    /// replica; cleared by the next sync that goes through.
    #[serde(default)]
    pub sync_refused: Option<SyncRefusal>,
    /// Set while the source has twice found the volume holding no files. Only the
    /// verdict lives here; size and newest file are scanned on demand. Cleared by the first scan finding a file.
    #[serde(default)]
    pub looks_empty: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncRefusal {
    pub from: String,
    pub to: String,
    pub reason: String,
    /// Unix seconds, stamped by the refusing node to keep `apply` clock free.
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrozenVolume {
    /// Node being edited; primary again on unfreeze.
    pub node: String,
    /// Instances the freeze stopped, so unfreeze starts exactly those.
    pub stopped: Vec<InstanceId>,
}

impl VolumeState {
    pub fn in_sync(&self) -> BTreeSet<&str> {
        self.replicas
            .iter()
            .filter(|(_, gen)| **gen == self.generation)
            .map(|(node, _)| node.as_str())
            .collect()
    }
}

/// A certificate distributed to every node so any can terminate TLS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CertificateState {
    pub fqdn: String,
    pub chain_pem: String,
    pub key_pem: String,
    pub not_after: i64,
    /// Unix seconds issued; 0 when unknown (stored before this field existed).
    #[serde(default)]
    pub issued_at: i64,
}

/// A DNS record clouduler created, tracked so ownership survives a leader change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DnsRecordState {
    pub fqdn: String,
    pub record_type: String,
    pub content: String,
    pub zone: String,
    /// Cloudflare's id, for later update or delete.
    pub provider_id: Option<String>,
}

/// A hostname's state before it has a live `certificates`/`dns_records` entry; set
/// as soon as it becomes desired so the ui has something to show, removed once
/// the real record lands (so presence there always means "ready").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReconcileStatus {
    Pending,
    Error { message: String, at: i64 },
}

/// A place restic can keep a drive's backup history; see `DriveBackupTarget`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupDestination {
    pub id: String,
    pub name: String,
    pub kind: BackupDestinationKind,
    /// Cluster secret holding the restic repo password (required for every backend).
    pub password_secret: String,
    /// Cluster secret holding backend credentials (ssh key for `Ssh`, key pair for `S3`); `None` for `NodeLocal`.
    #[serde(default)]
    pub credentials_secret: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "kind")]
pub enum BackupDestinationKind {
    /// A path on one node's disk. If that node does not hold the drive, the runner
    /// syncs the data there once before running restic locally, rather than adding
    /// a standing replica (a live mirror and point-in-time history are separate concerns).
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

/// One drive's backup schedule and retention against one destination.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveBackupTarget {
    pub id: String,
    pub drive: String,
    pub destination_id: String,
    pub enabled: bool,
    pub frequency_secs: u64,
    /// "HH:MM", 24h, node-local time; `None` means any time once `frequency_secs` has elapsed.
    #[serde(default)]
    pub time_of_day: Option<String>,
    #[serde(default)]
    pub keep_last: Option<u32>,
    #[serde(default)]
    pub keep_within_secs: Option<u64>,
    /// Keep the newest snapshot of each of the last N days/weeks/months/years (restic `--keep-*`).
    #[serde(default)]
    pub keep_daily: Option<u32>,
    #[serde(default)]
    pub keep_weekly: Option<u32>,
    #[serde(default)]
    pub keep_monthly: Option<u32>,
    #[serde(default)]
    pub keep_yearly: Option<u32>,
    /// Overrides the resolved stop-before-backup default when set (see
    /// `cl_config::stack::StackFile::effective_backup_stop_before`).
    #[serde(default)]
    pub stop_containers: Option<bool>,
    /// Unix seconds this target last started a run, to tell if `frequency_secs` elapsed; `None` before the first.
    #[serde(default)]
    pub last_run: Option<i64>,
}

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
    /// restic's `percent_done` from its last status line, updated in place by repeated
    /// `RecordBackupRun` submits for the same run id; `None` until the first.
    Running {
        #[serde(default)]
        percent: Option<f64>,
    },
    Succeeded {
        snapshot_id: String,
        bytes: u64,
        /// Set when the snapshot was taken but something after it failed (retention
        /// `forget --prune`, restarting containers). The run is a success; the operator should see the message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
    },
    Failed {
        error: String,
    },
}

// Hand-written so a raft log/snapshot from before `Running` grew `percent` (a bare
// unit variant, the JSON string `"Running"`) still replays: the derived
// `Deserialize` rejects it and crashes every node on startup. Routes through
// `serde_json::Value` (format-agnostic) to special-case the legacy string.
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
                #[serde(default)]
                warning: Option<String>,
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
            Current::Succeeded {
                snapshot_id,
                bytes,
                warning,
            } => Ok(BackupRunOutcome::Succeeded {
                snapshot_id,
                bytes,
                warning,
            }),
            Current::Failed { error } => Ok(BackupRunOutcome::Failed { error }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BackupTrigger {
    Schedule,
    Manual { by: String },
}

/// Newest runs kept per `(drive, destination_id)`; bounded like `MAX_OBJECT_BYTES` since history goes through the raft log and snapshots.
pub const MAX_BACKUP_RUNS_PER_TARGET: usize = 50;

/// What a restore covers: a drive, one volume, or every volume a service uses.
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

/// A restore submitted through raft. Whichever node can carry it out (the drive's
/// primary, or the holder of a node-local repository; see `backup_restore.rs`)
/// claims it (`Pending` -> `Running`) and executes, like `BackupRun`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestoreRequest {
    pub id: String,
    pub scope: RestoreScope,
    /// Destination to restore from. Required for `Drive`/`Volume`; for `Service` optional
    /// when resolvable (see `backup_restore::resolve_service_destination`).
    #[serde(default)]
    pub destination_id: Option<String>,
    #[serde(default)]
    pub snapshot: Option<String>,
    pub requested_at: i64,
    #[serde(default)]
    pub finished_at: Option<i64>,
    pub status: RestoreStatus,
    pub requested_by: String,
}

/// Newest requests kept, one bound across the whole set (restores are rare).
pub const MAX_RESTORE_REQUESTS: usize = 50;

/// Schema version of this build's `ClusterState`/`Request` shapes. Bump by hand
/// when a change needs a `crate::migrations` entry or is unsafe for a node behind
/// to have installed (see `store.rs`).
pub const CURRENT_SCHEMA_VERSION: u32 = 2;

/// How long `RecordCrashMove` keeps a node on an instance's crash-move list; also the window the scheduler avoids it.
pub const CRASH_MOVE_RETENTION_SECS: u64 = 30 * 60;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClusterState {
    pub nodes: BTreeMap<String, NodeState>,
    pub stacks: BTreeMap<String, StackState>,
    pub assignments: BTreeMap<InstanceId, Assignment>,
    pub volumes: BTreeMap<VolumeId, VolumeState>,
    #[serde(default)]
    pub cluster_config: Option<ClusterConfigEntry>,
    #[serde(default)]
    pub configs: BTreeMap<String, ConfigEntry>,
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretEntry>,
    pub certificates: BTreeMap<String, CertificateState>,
    pub dns_records: BTreeMap<String, DnsRecordState>,
    /// Pending/error status for hostnames with no `certificates` entry yet, by fqdn.
    #[serde(default)]
    pub cert_status: BTreeMap<String, ReconcileStatus>,
    /// Pending/error status for hostnames with no live `dns_records` entry yet, by fqdn.
    #[serde(default)]
    pub dns_status: BTreeMap<String, ReconcileStatus>,
    /// The cluster secret key wrapped once per node, keyed by node name.
    #[serde(default)]
    pub wrapped_keys: BTreeMap<String, WrappedKeyEntry>,
    /// Nodes the leader has stopped hearing from; observed, so kept apart from the operator-set `availability`.
    #[serde(default)]
    pub unreachable: BTreeSet<String>,
    /// Ids (`cl_config::Node::id`) removed through `RemoveNode`, a permanent tombstone:
    /// without it a leader whose in-memory config still lists the node would treat
    /// it as newly joined and undo the removal. Keyed by id so the name can be reused.
    #[serde(default)]
    pub removed: BTreeSet<uuid::Uuid>,
    /// Overlay subnet and address assignments, opaque json owned by cl-net so the address book can evolve without touching the state machine.
    #[serde(default)]
    pub overlay: Option<String>,
    /// Set only by `Store::emergency`'s offline write, never a normal `Request`: while
    /// it holds stack names, the scheduler places and starts only those stacks,
    /// leaving other assignments' `desired` untouched so nothing needs restoring
    /// when it clears. `ClearEmergency` is the only way out, submitted by
    /// `watch_leadership` once another voter is reachable (`QuorumStatus::reachable`).
    #[serde(default)]
    pub emergency_stacks: BTreeSet<String>,
    /// Stacks `clouduler prioritize` bumped ahead of others for starts and volume syncs. See `Request::SetPrioritized`.
    #[serde(default)]
    pub prioritized: BTreeSet<String>,
    #[serde(default)]
    pub restic_destinations: BTreeMap<String, BackupDestination>,
    #[serde(default)]
    pub restic_targets: BTreeMap<String, DriveBackupTarget>,
    #[serde(default)]
    pub restic_runs: BTreeMap<String, BackupRun>,
    #[serde(default)]
    pub restore_requests: BTreeMap<String, RestoreRequest>,
    /// Live container state as self-reported by each instance's node, keyed by
    /// instance. Filled by the scheduler's reconcile pass, so waiters can poll it
    /// instead of sleeping out a worst-case deadline. See `Request::ReportContainerStatus`.
    #[serde(default)]
    pub container_status: BTreeMap<InstanceId, ContainerStatus>,
    /// Image updates and redeploys in flight or recently finished, keyed by instance,
    /// written step by step by the working node so every node's api shows the same progress.
    #[serde(default)]
    pub instance_updates: BTreeMap<InstanceId, InstanceUpdate>,
    /// OpenTofu modules by stack then service. See `crate::terraform`.
    #[serde(default)]
    pub terraform_modules: BTreeMap<String, BTreeMap<String, crate::terraform::TerraformModule>>,
    /// Sealed tofu state by `terraform_key`.
    #[serde(default)]
    pub terraform_states: BTreeMap<String, crate::terraform::TerraformState>,
    /// Latest run per `terraform_key`.
    #[serde(default)]
    pub terraform_runs: BTreeMap<String, crate::terraform::TerraformRun>,
    /// Latest Apply run per `terraform_key`, kept apart from `terraform_runs` so a plan
    /// cannot hide a failed apply from the no-retry rule.
    #[serde(default)]
    pub terraform_last_apply: BTreeMap<String, crate::terraform::TerraformRun>,
    /// Pending manual Plan/Apply requests by `terraform_key`.
    #[serde(default)]
    pub terraform_requests: BTreeMap<String, crate::terraform::TerraformMode>,
    /// Fingerprint of the last successful Apply by `terraform_key`.
    #[serde(default)]
    pub terraform_applied: BTreeMap<String, String>,
    /// Secrets and configs each module's `outputs:` wrote, by `terraform_key`. Owned
    /// objects are deleted with the binding (unless another stack uses them).
    #[serde(default)]
    pub terraform_outputs: BTreeMap<String, BTreeSet<crate::terraform::OutputObject>>,
    /// Nodes a crash-looping instance was recently moved from, with the move's unix
    /// second. In raft because the owner changes with each move: node-local memory
    /// would start empty each time and let a service that crashes everywhere bounce
    /// forever. Written by `Request::RecordCrashMove`; cleared on spec change or
    /// instance removal; entries age out via the scheduler's window.
    #[serde(default)]
    pub crash_moves: BTreeMap<InstanceId, BTreeMap<String, u64>>,
    /// The digest an instance was pinned to before its current pin, so a broken rolling update can re-pin what worked.
    #[serde(default)]
    pub previous_image_digests: BTreeMap<InstanceId, String>,
    /// What `x-clouduler.auto_update` did about each instance's newest registry digest.
    /// Leader-written and in raft so a new leader neither restarts a delay countdown
    /// nor retries a failed update. See `AutoUpdateRecord`.
    #[serde(default)]
    pub auto_updates: BTreeMap<InstanceId, AutoUpdateRecord>,
    /// What registries last said about each declared image reference (`nginx:1.25`).
    /// Filled by the leader's timer and the Updates page Refresh; reading never hits a registry. See `ImageCheck`.
    #[serde(default)]
    pub image_checks: BTreeMap<String, ImageCheck>,
    /// Unix seconds the last registry check finished; 0 before the first.
    #[serde(default)]
    pub image_checks_at: i64,
    /// Node holding each served drive's authoritative copy, by drive; no entry means
    /// no failover yet (first reachable `served_by` candidate serves). See `served`.
    #[serde(default)]
    pub served: BTreeMap<String, crate::served::ServedAuthority>,
    /// Volumes deleted on purpose, with the nodes still to remove their copy; the entry goes with the last.
    #[serde(default)]
    pub volume_removals: BTreeMap<VolumeId, BTreeSet<String>>,
    /// Logged-in browser sessions keyed by a hash of the session id (so the raft log
    /// holds no usable cookie); replicated to survive restarts and leader changes.
    /// Not in the git backup. See `Request::PutSession`.
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionRecord>,
    /// Sessions of the reverse proxy's `authenticated` login keyed by a hash of the
    /// cookie: one per (browser, hostname), including the portal's own. Same storage
    /// rules as `sessions`. See `Request::PutProxySession`.
    #[serde(default)]
    pub proxy_sessions: BTreeMap<String, ProxySessionRecord>,
    /// Single-use tokens from the login portal to a protected hostname's cookie endpoint, keyed by token hash.
    #[serde(default)]
    pub proxy_tokens: BTreeMap<String, ProxyTokenRecord>,
    /// The in-app alert feed (open alerts plus history) by record id; leader-written, not in the git backup.
    #[serde(default)]
    pub alerts: BTreeMap<String, AlertRecord>,
    /// Signals nodes pushed for the engine to turn into records; bounded by `MAX_ALERT_INBOX`, drained by the leader.
    #[serde(default)]
    pub alert_inbox: BTreeMap<String, AlertSignal>,
    #[serde(default)]
    pub alert_destinations: BTreeMap<String, DestinationHealth>,
    /// Incremented on every applied command, so watchers notice changes without diffing.
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub subject: String,
    pub username: String,
    pub admin: bool,
    /// Unix seconds after which the session no longer counts.
    pub expires_at: i64,
}

/// `ProxySessionRecord::host` of a saved client address (`ip:<network>:<ip>`) rather than a cookie session; no real hostname starts with it.
pub const REMEMBERED_IP_HOST_PREFIX: &str = "ip:";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxySessionRecord {
    /// Hostname the cookie was issued for (lowercase, no port); honoured only there.
    pub host: String,
    pub subject: String,
    pub username: String,
    #[serde(default)]
    pub groups: Vec<String>,
    pub expires_at: i64,
    /// Key of the portal session this was handed out from, empty for a portal
    /// session; removing the parent removes it (sign-out ends every app cookie).
    #[serde(default)]
    pub parent: String,
}

/// A one-time pass from the login portal to one protected hostname. See `Request::RedeemProxyToken`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyTokenRecord {
    pub host: String,
    pub subject: String,
    pub username: String,
    #[serde(default)]
    pub groups: Vec<String>,
    pub session_expires_at: i64,
    pub expires_at: i64,
    #[serde(default)]
    pub parent: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContainerStatus {
    /// The reporting node, so a fresh report replaces exactly its own prior entries (including dropped instances).
    pub node: String,
    pub running: bool,
    /// Revision of each cluster secret the container was created with (from its
    /// `clouduler.secrets` label). Empty both for no secrets and for pre-label
    /// containers, which are indistinguishable here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secret_revisions: BTreeMap<String, u64>,
    /// Same for the read-only configs (`clouduler.configs` label). A `write: true` config is live-edited and
    /// never listed. Empty also for pre-label containers, which are never treated as stale.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config_revisions: BTreeMap<String, u64>,
}

/// Where one instance's image update or redeploy stands; written via
/// `Request::SetInstanceUpdate` at each docker step, cleared by the owner a while after a terminal phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceUpdate {
    /// The node whose docker runs the steps (the instance's node when the update began).
    pub node: String,
    pub phase: UpdatePhase,
    pub started_at: i64,
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageCheck {
    /// The registry's digest for the reference; kept from the previous check when the latest lookup failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_digest: Option<String>,
    pub checked_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoUpdateRecord {
    /// The registry digest this record is about; a different one replaces it and restarts the countdown.
    pub digest: String,
    pub detected_at: i64,
    pub outcome: AutoUpdateOutcome,
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoUpdateOutcome {
    /// Seen, and waiting out the configured delay.
    Waiting,
    /// Handed to the update pipeline; its progress is in `instance_updates`.
    Queued,
    /// The instance restarted on the new image.
    Updated,
    /// The update failed. Not retried for this digest.
    Failed,
    /// Mode `notify`: an `update.available` alert was raised.
    Notified,
}

/// The steps of an update in order; the old container keeps serving through `Queued` and `Pulling`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePhase {
    /// Accepted, waiting its turn (per node, one image download and one restart at a time).
    Queued,
    /// Downloading the new image while the old container keeps running.
    Pulling,
    /// Stopping and removing the old container.
    Stopping,
    /// Creating and starting the new container.
    Starting,
    /// The new container is started.
    Started,
    Failed,
}

impl UpdatePhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, UpdatePhase::Started | UpdatePhase::Failed)
    }
}

/// A command submitted through raft; every mutation of cluster state is one of these.
/// `Deserialize` is hand-written (see its impl) so an unknown variant from a newer node becomes `Unknown`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Request {
    /// Replace the cluster config file on every node. `expect_generation` is the
    /// generation the writer read (0 if none stored); a mismatch is refused rather than overwriting.
    PutClusterConfig {
        yaml: String,
        expect_generation: u64,
    },
    UpsertNode(NodeState),
    SetAvailability {
        node: String,
        availability: Availability,
    },
    /// Submitted by the leader once a node is silent past `failover.node_timeout`, or answers again.
    SetReachable {
        node: String,
        reachable: bool,
    },
    RemoveNode {
        node: String,
    },
    /// Tombstone an id without touching the roster, so `reap_removed_nodes` drops it
    /// from raft membership on its own timer. For an entry that changes identity
    /// but keeps its name (re-provisioned via `init --force`, or a node demoted by
    /// `force_quorum` returning under a new id); `RemoveNode` would delete the live entry.
    RetireNodeId {
        id: uuid::Uuid,
    },
    DeployStack {
        name: String,
        spec: String,
        /// Unix seconds of the deploy, stamped by the caller so `apply` needs no clock.
        #[serde(default)]
        at: i64,
    },
    RemoveStack {
        name: String,
    },
    /// Record which cluster objects a stack owns so removing it cleans them up; sent after they are written.
    SetStackObjects {
        name: String,
        configs: BTreeSet<String>,
        secrets: BTreeSet<String>,
        #[serde(default)]
        used_configs: BTreeSet<String>,
        #[serde(default)]
        used_secrets: BTreeSet<String>,
    },
    /// Place an instance; `drives` lets the state machine refuse a node lacking any.
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
    /// Forgets a failed move. Refused unless the record is `Failed`.
    ClearMigration {
        instance: InstanceId,
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
    /// Records (or with `None` clears) that the source refused to replicate to avoid wiping a replica.
    SetVolumeSyncRefusal {
        id: VolumeId,
        refusal: Option<SyncRefusal>,
    },
    /// Deletes a volume, files included, on every node implementing its drive. Refused while frozen or mounted.
    RemoveVolume {
        id: VolumeId,
    },
    AckVolumeRemoved {
        id: VolumeId,
        node: String,
    },
    SetVolumeEmpty {
        id: VolumeId,
        empty: bool,
    },
    FreezeVolume {
        id: VolumeId,
        node: String,
        stopped: Vec<InstanceId>,
    },
    /// Resumes replication; the frozen node's copy becomes the new generation.
    UnfreezeVolume {
        id: VolumeId,
    },
    StoreCertificate(CertificateState),
    RemoveCertificate {
        fqdn: String,
    },
    /// A hostname's certificate is being ordered or failed; superseded by `StoreCertificate`.
    SetCertStatus {
        fqdn: String,
        status: ReconcileStatus,
    },
    UpsertDnsRecord(DnsRecordState),
    RemoveDnsRecord {
        key: String,
    },
    /// A hostname's dns record is being reconciled or failed; superseded by `UpsertDnsRecord`.
    SetDnsStatus {
        fqdn: String,
        status: ReconcileStatus,
    },
    /// Drops a hostname's cert/dns reconcile status, which only a successful
    /// store/upsert otherwise supersedes (one for a departed hostname would linger).
    ClearReconcileStatus {
        fqdn: String,
    },
    PutConfig {
        name: String,
        content: String,
        #[serde(default)]
        owner: Option<String>,
    },
    /// `PutConfig` applied only if the current revision is still `expect_revision`
    /// (0 = does not exist yet), so the loser of a read-modify-write races gets an error.
    PutConfigCas {
        name: String,
        content: String,
        #[serde(default)]
        owner: Option<String>,
        expect_revision: u64,
    },
    RemoveConfig {
        name: String,
    },
    /// Sealed by the caller: plaintext never enters the raft log.
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
    /// Written by the node holding the plaintext key once `node` publishes a pubkey
    /// with no (or a stale, after rotation) wrapped copy.
    PutWrappedKey {
        node: String,
        entry: WrappedKeyEntry,
    },
    /// Overlay address book; allocation goes through raft so two nodes never hand out the same address.
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
    /// Submitted by the leader when a target is due, claiming the tick so no other term double-fires.
    SetBackupTargetLastRun {
        id: String,
        at: i64,
    },
    /// Insert or update by id (`Running` -> `Succeeded`/`Failed`).
    RecordBackupRun(BackupRun),
    /// Submitted by an api handler; always inserted `Pending`, since only the claiming node may advance it.
    PutRestoreRequest(RestoreRequest),
    /// Advances a restore's status: a claim (`Pending` -> `Running`) or a terminal
    /// outcome. The claim is an optimistic lock: of two racing claimers only the
    /// first commits (see `backup_restore`).
    SetRestoreStatus {
        id: String,
        status: RestoreStatus,
        /// Unix seconds from the caller (keeps `apply` pure); used as `finished_at` when `status` is terminal.
        at: i64,
    },
    /// Self-reported each reconcile pass: state of every instance assigned to
    /// `node`, wholesale-replacing its prior `container_status` entries so moved or removed instances drop out.
    ReportContainerStatus {
        node: String,
        running: BTreeMap<InstanceId, bool>,
        /// Per instance, the secret revisions its container was created with; optional for older nodes' reports.
        #[serde(default)]
        secret_revisions: BTreeMap<InstanceId, BTreeMap<String, u64>>,
        /// Likewise for read-only configs; optional for older nodes' reports.
        #[serde(default)]
        config_revisions: BTreeMap<InstanceId, BTreeMap<String, u64>>,
    },
    /// Record the step an update or redeploy reached; `at` is caller-supplied unix
    /// seconds. Rejected for an instance with no assignment.
    SetInstanceUpdate {
        instance: InstanceId,
        node: String,
        phase: UpdatePhase,
        #[serde(default)]
        error: Option<String>,
        at: i64,
    },
    ClearInstanceUpdate {
        instance: InstanceId,
    },
    /// Record what `x-clouduler.auto_update` did about `digest`; the same digest keeps
    /// its `detected_at`, a new one starts afresh at `at` (unix seconds).
    SetAutoUpdate {
        instance: InstanceId,
        digest: String,
        outcome: AutoUpdateOutcome,
        #[serde(default)]
        error: Option<String>,
        at: i64,
    },
    /// Replace the registry check results wholesale (undeclared images drop out);
    /// `at` becomes `image_checks_at`. A failed entry keeps the previous digest.
    SetImageChecks {
        checks: BTreeMap<String, ImageCheck>,
        at: i64,
    },
    /// Drops every recorded failure (failed backup runs, restores, updates,
    /// migrations, cert/DNS errors, failed alert deliveries); running, pending and succeeded records stay.
    ClearErrors,
    /// Store a login session under `key` (a hash of the session id); `now` is caller
    /// supplied, and expired sessions are dropped at the same time.
    PutSession {
        key: String,
        session: SessionRecord,
        now: i64,
    },
    RemoveSession {
        key: String,
    },
    PruneSessions {
        now: i64,
    },
    /// Queue a node's signal for the alert engine; the inbox is bounded, oldest dropped first.
    PushAlertSignal(AlertSignal),
    TakeAlertSignals {
        ids: Vec<String>,
    },
    PutAlert(AlertRecord),
    RemoveAlert {
        id: String,
    },
    /// Delete resolved and one-shot records last touched before `before`, then the oldest until at most `max` remain.
    PruneAlerts {
        before: i64,
        max: usize,
    },
    SetDestinationHealth {
        name: String,
        health: DestinationHealth,
    },
    RemoveDestinationHealth {
        name: String,
    },
    /// Store a reverse proxy session under `key` (hash of the cookie); also sweeps expired ones, like `PutSession`.
    PutProxySession {
        key: String,
        session: ProxySessionRecord,
        now: i64,
    },
    RemoveProxySession {
        key: String,
    },
    PutProxyToken {
        key: String,
        token: ProxyTokenRecord,
        now: i64,
    },
    /// Atomically spend the token under `token_key`: it must exist, be unexpired and
    /// issued for `host`; then a session for `host` exists under `session_key`. On
    /// failure nothing changes. Of two racing redeems exactly one succeeds.
    RedeemProxyToken {
        token_key: String,
        host: String,
        session_key: String,
        now: i64,
    },
    /// Remember that `instance` was moved off `from` for crashing; `at` is caller
    /// supplied, and entries older than `CRASH_MOVE_RETENTION_SECS` are dropped.
    RecordCrashMove {
        instance: InstanceId,
        from: String,
        at: u64,
    },
    /// Written on leader election so the log always has a fresh entry to commit.
    NoOp,
    /// Ends emergency mode (`ClusterState::emergency_stacks`). Submitted by
    /// `watch_leadership` once another voter is reachable, not by an operator.
    ClearEmergency,
    /// Puts `stack` ahead of all others (`on`) or back at its own priority (`!on`).
    /// Turning on for an undeployed stack is refused.
    SetPrioritized {
        stack: String,
        on: bool,
    },
    /// Changes who holds a served drive, or a handover. See `served::ServedOp`.
    Served {
        drive: String,
        op: crate::served::ServedOp,
    },
    /// Replaces a stack's OpenTofu modules (service -> source).
    SetStackTerraform {
        stack: String,
        modules: BTreeMap<String, String>,
    },
    /// Sealed by the caller; compare-and-set on `expect_serial`.
    PutTerraformState {
        key: String,
        sealed: String,
        key_fingerprint: String,
        expect_serial: u64,
    },
    /// Starts (Running: the claim) or finishes a run; see `crate::terraform`.
    RecordTerraformRun {
        run: crate::terraform::TerraformRun,
    },
    /// The run's module outputs, written to cluster secrets and configs; see
    /// `ClusterState::apply_terraform_outputs`. `claims` take ownership of unchanged,
    /// already existing objects without rewriting them.
    PutTerraformOutputs {
        key: String,
        run_id: String,
        secrets: Vec<crate::terraform::TerraformSecretOutput>,
        configs: Vec<crate::terraform::TerraformConfigOutput>,
        #[serde(default)]
        claims: Vec<crate::terraform::OutputObject>,
    },
    /// A manual Plan/Apply, picked up by the leader's runner.
    RequestTerraformRun {
        key: String,
        mode: crate::terraform::TerraformMode,
    },
    /// Stand-in for a variant this (older) binary does not recognize, produced only
    /// by `Deserialize` for a genuinely unknown tag. `dispatch` treats it as a
    /// no-op without bumping `version`; the real effect lands once this node is
    /// upgraded and replays a snapshot or the entry. Holds the raw JSON and is
    /// `untagged` so re-serializing reproduces the payload verbatim (an old leader
    /// re-replicating it); the legacy bare string `"Unknown"` round trips too.
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

/// Every variant name except `Unknown`: tells "never-heard-of tag" (falls back to
/// `Unknown`) from "known tag, malformed body" (hard error). Extend with each
/// new variant, alongside `request_mirror` (checked by
/// `every_current_request_variant_round_trips_and_is_not_unknown`).
const KNOWN_REQUEST_VARIANTS: &[&str] = &[
    "PutClusterConfig",
    "UpsertNode",
    "SetAvailability",
    "SetReachable",
    "RemoveNode",
    "RetireNodeId",
    "DeployStack",
    "RemoveStack",
    "SetStackObjects",
    "Assign",
    "Unassign",
    "SetDesiredState",
    "SetImageDigest",
    "BeginMigration",
    "AdvanceMigration",
    "ClearMigration",
    "UpsertVolume",
    "SetVolumeReplica",
    "TouchVolume",
    "SetVolumeSyncRefusal",
    "SetVolumeEmpty",
    "RemoveVolume",
    "AckVolumeRemoved",
    "FreezeVolume",
    "UnfreezeVolume",
    "StoreCertificate",
    "RemoveCertificate",
    "SetCertStatus",
    "UpsertDnsRecord",
    "RemoveDnsRecord",
    "SetDnsStatus",
    "ClearReconcileStatus",
    "PutConfig",
    "PutConfigCas",
    "RemoveConfig",
    "PutSecret",
    "RemoveSecret",
    "PutWrappedKey",
    "SetOverlayAllocations",
    "PutBackupDestination",
    "RemoveBackupDestination",
    "PutBackupTarget",
    "RemoveBackupTarget",
    "SetBackupTargetLastRun",
    "RecordBackupRun",
    "PutRestoreRequest",
    "SetRestoreStatus",
    "ReportContainerStatus",
    "SetInstanceUpdate",
    "ClearInstanceUpdate",
    "SetAutoUpdate",
    "SetImageChecks",
    "ClearErrors",
    "PutSession",
    "RemoveSession",
    "PruneSessions",
    "PushAlertSignal",
    "TakeAlertSignals",
    "PutAlert",
    "RemoveAlert",
    "PruneAlerts",
    "SetDestinationHealth",
    "RemoveDestinationHealth",
    "PutProxySession",
    "RemoveProxySession",
    "PutProxyToken",
    "RedeemProxyToken",
    "RecordCrashMove",
    "NoOp",
    "ClearEmergency",
    "SetPrioritized",
    "Served",
    "SetStackTerraform",
    "PutTerraformState",
    "RecordTerraformRun",
    "PutTerraformOutputs",
    "RequestTerraformRun",
];

// Mirrors `Request` field-for-field so `#[derive(Deserialize)]` can generate the
// per-variant parsing under `serde(remote)`. Every new `Request` variant must be
// added here too: nothing forces it, and drift would make a known variant
// deserialize as `Unknown` (the `every_variant_name_round_trips` test is the tripwire).
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
            #[serde(default)]
            at: i64,
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
        ClearMigration {
            instance: InstanceId,
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
        SetVolumeSyncRefusal {
            id: VolumeId,
            refusal: Option<SyncRefusal>,
        },
        SetVolumeEmpty {
            id: VolumeId,
            empty: bool,
        },
        RemoveVolume {
            id: VolumeId,
        },
        AckVolumeRemoved {
            id: VolumeId,
            node: String,
        },
        FreezeVolume {
            id: VolumeId,
            node: String,
            stopped: Vec<InstanceId>,
        },
        UnfreezeVolume {
            id: VolumeId,
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
        ClearReconcileStatus {
            fqdn: String,
        },
        PutConfig {
            name: String,
            content: String,
            #[serde(default)]
            owner: Option<String>,
        },
        PutConfigCas {
            name: String,
            content: String,
            #[serde(default)]
            owner: Option<String>,
            expect_revision: u64,
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
        ReportContainerStatus {
            node: String,
            running: BTreeMap<InstanceId, bool>,
            #[serde(default)]
            secret_revisions: BTreeMap<InstanceId, BTreeMap<String, u64>>,
            #[serde(default)]
            config_revisions: BTreeMap<InstanceId, BTreeMap<String, u64>>,
        },
        SetInstanceUpdate {
            instance: InstanceId,
            node: String,
            phase: UpdatePhase,
            #[serde(default)]
            error: Option<String>,
            at: i64,
        },
        ClearInstanceUpdate {
            instance: InstanceId,
        },
        SetAutoUpdate {
            instance: InstanceId,
            digest: String,
            outcome: AutoUpdateOutcome,
            #[serde(default)]
            error: Option<String>,
            at: i64,
        },
        SetImageChecks {
            checks: BTreeMap<String, ImageCheck>,
            at: i64,
        },
        ClearErrors,
        PutSession {
            key: String,
            session: SessionRecord,
            now: i64,
        },
        RemoveSession {
            key: String,
        },
        PruneSessions {
            now: i64,
        },
        PushAlertSignal(AlertSignal),
        TakeAlertSignals {
            ids: Vec<String>,
        },
        PutAlert(AlertRecord),
        RemoveAlert {
            id: String,
        },
        PruneAlerts {
            before: i64,
            max: usize,
        },
        SetDestinationHealth {
            name: String,
            health: DestinationHealth,
        },
        RemoveDestinationHealth {
            name: String,
        },
        PutProxySession {
            key: String,
            session: ProxySessionRecord,
            now: i64,
        },
        RemoveProxySession {
            key: String,
        },
        PutProxyToken {
            key: String,
            token: ProxyTokenRecord,
            now: i64,
        },
        RedeemProxyToken {
            token_key: String,
            host: String,
            session_key: String,
            now: i64,
        },
        RecordCrashMove {
            instance: InstanceId,
            from: String,
            at: u64,
        },
        NoOp,
        ClearEmergency,
        SetPrioritized {
            stack: String,
            on: bool,
        },
        Served {
            drive: String,
            op: crate::served::ServedOp,
        },
        SetStackTerraform {
            stack: String,
            modules: BTreeMap<String, String>,
        },
        PutTerraformState {
            key: String,
            sealed: String,
            key_fingerprint: String,
            expect_serial: u64,
        },
        RecordTerraformRun {
            run: crate::terraform::TerraformRun,
        },
        PutTerraformOutputs {
            key: String,
            run_id: String,
            secrets: Vec<crate::terraform::TerraformSecretOutput>,
            configs: Vec<crate::terraform::TerraformConfigOutput>,
            #[serde(default)]
            claims: Vec<crate::terraform::OutputObject>,
        },
        RequestTerraformRun {
            key: String,
            mode: crate::terraform::TerraformMode,
        },
        // `Unknown` is absent: it is only ever the fallback when nothing matches.
    }
}

// Hand-written like `BackupRunOutcome`'s impl, but for forward compatibility: a
// tag from a newer node must not fail. Delegates to `request_mirror::RequestMirror`
// (`serde(remote)`) and falls back to `Request::Unknown` when the tag is not in
// `KNOWN_REQUEST_VARIANTS` (a known tag with a bad body is an error). Also
// covers raft replication itself, since openraft's `Entry` embeds `Request`:
// this protects an old follower from a newer leader's new command.
impl<'de> Deserialize<'de> for Request {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        // The externally-tagged variant name: bare string or sole object key.
        let tag = match &value {
            serde_json::Value::String(s) => Some(s.as_str()),
            serde_json::Value::Object(map) if map.len() == 1 => {
                map.keys().next().map(|k| k.as_str())
            }
            _ => None,
        };
        match tag {
            // Only a never-heard-of tag is "from a newer node" (old logs wrote "Unknown").
            Some(tag) if tag == "Unknown" || !KNOWN_REQUEST_VARIANTS.contains(&tag) => {
                Ok(Request::Unknown(value))
            }
            // A known tag with an unparseable body is a bug or corruption, not version
            // skew: fail loudly rather than no-op here while other nodes apply it.
            _ => {
                request_mirror::RequestMirror::deserialize(&value).map_err(serde::de::Error::custom)
            }
        }
    }
}

/// Result of applying a command; failures are values, since a rejected command is still a committed entry.
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
    /// Keep at most `max` alert records: resolved and one-shot first, oldest first, open ones last.
    fn trim_alerts(&mut self, max: usize) {
        while self.alerts.len() > max {
            let victim = self
                .alerts
                .values()
                .min_by(|a, b| {
                    (a.state.is_open(), a.last_at, &a.id).cmp(&(
                        b.state.is_open(),
                        b.last_at,
                        &b.id,
                    ))
                })
                .map(|a| a.id.clone());
            match victim {
                Some(id) => self.alerts.remove(&id),
                None => break,
            };
        }
    }

    fn sweep_proxy_auth(&mut self, now: i64) {
        self.proxy_sessions.retain(|_, s| s.expires_at > now);
        self.proxy_tokens.retain(|_, t| t.expires_at > now);
    }

    /// Apply one command. Must stay deterministic: no clocks, randomness or IO.
    pub fn apply(&mut self, req: &Request) -> Response {
        // An `Unknown` command must not bump `version` (it drives the SSE stream, lag
        // readings and snapshot ids); it still succeeds so the entry counts as applied.
        if matches!(req, Request::Unknown(_)) {
            let _ = self.dispatch(req);
            return Response::ok(self.version);
        }
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

            Request::Unknown(_) => {
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

            Request::PutConfigCas {
                name,
                content,
                owner,
                expect_revision,
            } => {
                let current = self.configs.get(name).map(|c| c.revision).unwrap_or(0);
                if current != *expect_revision {
                    return Err(format!(
                        "config {name} changed (revision {current}, expected {expect_revision})"
                    ));
                }
                self.dispatch(&Request::PutConfig {
                    name: name.clone(),
                    content: content.clone(),
                    owner: owner.clone(),
                })
            }

            Request::RemoveConfig { name } => {
                if let Some(user) = self.config_in_use(name) {
                    return Err(format!("config {name} is still used by {user}"));
                }
                self.configs.remove(name);
                self.forget_output_object(&crate::terraform::OutputObject::config(name));
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
                self.forget_output_object(&crate::terraform::OutputObject::secret(name));
                Ok(())
            }

            Request::PutWrappedKey { node, entry } => {
                let current = self
                    .nodes
                    .get(node)
                    .ok_or_else(|| format!("unknown node {node}"))?;
                if current.node_pubkey.as_deref() != Some(entry.target_pubkey.as_str()) {
                    return Err(format!(
                        "node {node} has not published the public key this was wrapped for"
                    ));
                }
                self.wrapped_keys.insert(node.clone(), entry.clone());
                Ok(())
            }

            Request::SetOverlayAllocations { allocations } => {
                self.overlay = Some(allocations.clone());
                Ok(())
            }

            Request::ClearEmergency => {
                self.emergency_stacks.clear();
                Ok(())
            }

            Request::SetPrioritized { stack, on } => {
                if !on {
                    self.prioritized.remove(stack);
                } else if self.stacks.contains_key(stack) {
                    self.prioritized.insert(stack.clone());
                } else {
                    return Err(format!("no stack named {stack}"));
                }
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
                    // A removed node must not stay a volume's primary: hand it to the replica
                    // furthest along (highest generation, BTree order for ties, so all nodes agree).
                    // Never refused over this (that would wedge removing a dead node); with no
                    // replica left it stays, which health reports as "primary not a member".
                    if volume.primary == *node {
                        let mut best: Option<(&String, u64)> = None;
                        for (candidate, generation) in &volume.replicas {
                            if best.is_none_or(|(_, g)| *generation > g) {
                                best = Some((candidate, *generation));
                            }
                        }
                        if let Some((candidate, _)) = best {
                            volume.primary = candidate.clone();
                        }
                    }
                }
                self.container_status
                    .retain(|_, status| &status.node != node);
                Ok(())
            }

            Request::RetireNodeId { id } => {
                self.removed.insert(*id);
                Ok(())
            }

            Request::DeployStack { name, spec, at } => {
                let revision = self.stacks.get(name).map(|s| s.revision + 1).unwrap_or(1);
                let previous = self.stacks.get(name);
                // A changed spec may have fixed the crash, so every node is worth retrying.
                if previous.is_some_and(|p| p.spec != *spec) {
                    self.crash_moves.retain(|id, _| &id.stack != name);
                }
                let configs = previous.map(|s| s.configs.clone()).unwrap_or_default();
                let secrets = previous.map(|s| s.secrets.clone()).unwrap_or_default();
                let used_configs = previous.map(|s| s.used_configs.clone()).unwrap_or_default();
                let used_secrets = previous.map(|s| s.used_secrets.clone()).unwrap_or_default();
                // An unchanged redeploy keeps the older "previous" so the diff shows the last real change.
                let previous_spec = match previous {
                    Some(p) if p.spec != *spec => Some(p.spec.clone()),
                    Some(p) => p.previous_spec.clone(),
                    None => None,
                };
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
                        deployed_at: *at,
                        previous_spec,
                    },
                );
                // Objects whose output binding went away are released before the module itself.
                self.release_outputs_for_spec(name, spec);
                // A service that left the spec forgets its terraform module and state.
                self.prune_terraform_for_spec(name, spec);
                Ok(())
            }

            Request::RemoveStack { name } => {
                // Drop the stack first so the in-use check below sees it gone.
                let removed = self.stacks.remove(name);
                self.prioritized.remove(name);
                self.assignments.retain(|id, _| &id.stack != name);
                self.instance_updates.retain(|id, _| &id.stack != name);
                self.auto_updates.retain(|id, _| &id.stack != name);
                self.crash_moves.retain(|id, _| &id.stack != name);
                self.drop_terraform_stack(name);
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
                // Carry over from the assignment being replaced: the pin (a node move does not
                // change the image, and losing it would drift to whatever the tag resolves to
                // next restart) and `desired` (re-placing an operator-stopped instance must not
                // start it; a new assignment starts Running). `migration` is kept only when
                // the node is unchanged: a re-Assign elsewhere supersedes any move in flight,
                // and a stale one would let a driver keep advancing it (`Switching` would
                // repoint the assignment back at the old target).
                let previous = self.assignments.get(instance);
                let image_digest = previous.and_then(|a| a.image_digest.clone());
                let desired = previous.map(|a| a.desired).unwrap_or(DesiredState::Running);
                let migration = previous
                    .filter(|a| a.node == *node)
                    .and_then(|a| a.migration.clone());
                self.assignments.insert(
                    instance.clone(),
                    Assignment {
                        instance: instance.clone(),
                        node: node.clone(),
                        desired,
                        image_digest,
                        migration,
                    },
                );
                Ok(())
            }

            Request::Unassign { instance } => {
                self.assignments.remove(instance);
                self.crash_moves.remove(instance);
                self.instance_updates.remove(instance);
                self.auto_updates.remove(instance);
                self.previous_image_digests.remove(instance);
                Ok(())
            }

            Request::SetDesiredState { instance, desired } => {
                self.assignment_mut(instance)?.desired = *desired;
                Ok(())
            }

            Request::SetImageDigest { instance, digest } => {
                let assignment = self.assignment_mut(instance)?;
                let old = assignment.image_digest.replace(digest.clone());
                // Re-pinning the same digest must not erase what the last real change replaced.
                if let Some(old) = old.filter(|old| old != digest) {
                    self.previous_image_digests.insert(instance.clone(), old);
                }
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

                // The phase already on record succeeds, so a driver retrying a submit it never
                // saw answered can tell "already done" from "not allowed".
                if &migration.phase == phase {
                    return Ok(());
                }

                // Failure can interrupt anywhere; forward progress must follow phase order so a stale message cannot rewind.
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
                    // Switching is the point of no return: the target owns the instance.
                    MigrationPhase::Switching => assignment.node = migration.to.clone(),
                    MigrationPhase::Done => assignment.migration = None,
                    _ => {}
                }
                Ok(())
            }

            Request::ClearMigration { instance } => {
                let assignment = self.assignment_mut(instance)?;
                match &assignment.migration {
                    Some(Migration {
                        phase: MigrationPhase::Failed { .. },
                        ..
                    }) => {
                        assignment.migration = None;
                        Ok(())
                    }
                    Some(_) => Err(format!("{instance} is still migrating")),
                    None => Err(format!("{instance} has no failed move to clear")),
                }
            }

            Request::UpsertVolume { id, primary } => {
                if !self.node_implements(primary, &id.drive) {
                    return Err(format!(
                        "node {primary} does not implement drive {}",
                        id.drive
                    ));
                }
                self.volumes
                    .entry(id.clone())
                    .or_insert_with(|| VolumeState {
                        id: id.clone(),
                        generation: 1,
                        primary: primary.clone(),
                        replicas: BTreeMap::from([(primary.clone(), 1)]),
                        frozen: None,
                        sync_refused: None,
                        looks_empty: false,
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
                    return Err(format!(
                        "node {primary} does not implement drive {}",
                        id.drive
                    ));
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

            Request::SetVolumeSyncRefusal { id, refusal } => {
                let volume = self
                    .volumes
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown volume {id}"))?;
                volume.sync_refused = refusal.clone();
                Ok(())
            }

            Request::SetVolumeEmpty { id, empty } => {
                let volume = self
                    .volumes
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown volume {id}"))?;
                volume.looks_empty = *empty;
                Ok(())
            }

            Request::RemoveVolume { id } => {
                let volume = self
                    .volumes
                    .get(id)
                    .ok_or_else(|| format!("unknown volume {id}"))?;
                if id.volume == "." {
                    return Err("a whole-drive mount is not a volume that can be deleted".into());
                }
                if volume.frozen.is_some() {
                    return Err(format!("{id} is frozen"));
                }
                let users = instances_using_volume(self, id);
                if !users.is_empty() {
                    let names: Vec<String> = users.iter().map(ToString::to_string).collect();
                    return Err(format!(
                        "{id} is still mounted by {}; remove it from the stack first",
                        names.join(", ")
                    ));
                }
                let stacks = stacks_declaring_volume(self, id);
                if !stacks.is_empty() {
                    return Err(format!(
                        "{id} is used by stack {}; remove it from the stack first",
                        stacks.join(", ")
                    ));
                }
                let holders: BTreeSet<String> = self
                    .nodes
                    .keys()
                    .filter(|n| self.node_implements(n, &id.drive))
                    .cloned()
                    .collect();
                self.volumes.remove(id);
                if !holders.is_empty() {
                    self.volume_removals.insert(id.clone(), holders);
                }
                Ok(())
            }

            Request::AckVolumeRemoved { id, node } => {
                if let Some(pending) = self.volume_removals.get_mut(id) {
                    pending.remove(node);
                    if pending.is_empty() {
                        self.volume_removals.remove(id);
                    }
                }
                Ok(())
            }

            Request::FreezeVolume { id, node, stopped } => {
                if !self.node_implements(node, &id.drive) {
                    return Err(format!("node {node} does not implement drive {}", id.drive));
                }
                let volume = self
                    .volumes
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown volume {id}"))?;
                if let Some(frozen) = &volume.frozen {
                    return Err(format!("{id} is already frozen on {}", frozen.node));
                }
                volume.frozen = Some(FrozenVolume {
                    node: node.clone(),
                    stopped: stopped.clone(),
                });
                Ok(())
            }

            Request::UnfreezeVolume { id } => {
                let volume = self
                    .volumes
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown volume {id}"))?;
                let frozen = volume
                    .frozen
                    .take()
                    .ok_or_else(|| format!("{id} is not frozen"))?;
                volume.generation += 1;
                volume.primary = frozen.node.clone();
                volume.replicas.insert(frozen.node, volume.generation);
                Ok(())
            }

            Request::PutBackupDestination(destination) => {
                validate_object_name(&destination.id)?;
                crate::backup_validate::validate_destination(destination)?;
                if let BackupDestinationKind::NodeLocal { node, .. } = &destination.kind {
                    if !self.nodes.contains_key(node) {
                        return Err(format!("unknown node {node}"));
                    }
                }
                self.restic_destinations
                    .insert(destination.id.clone(), destination.clone());
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
                if target.frequency_secs == 0 {
                    // Would be due on every scheduler poll, starting a new run the instant the last finished.
                    return Err("frequency_secs must be at least 1".to_string());
                }
                if !self.drive_known(&target.drive) {
                    return Err(format!("unknown drive {}", target.drive));
                }
                if !self
                    .restic_destinations
                    .contains_key(&target.destination_id)
                {
                    return Err(format!(
                        "unknown backup destination {}",
                        target.destination_id
                    ));
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
                self.restic_targets
                    .insert(target.id.clone(), target.clone());
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
                // Progress/finish for a run's own id is always allowed; only a new run competing for the target is rejected.
                if matches!(run.outcome, BackupRunOutcome::Running { .. })
                    && self.restic_runs.values().any(|r| {
                        r.target_id == run.target_id
                            && r.id != run.id
                            && matches!(r.outcome, BackupRunOutcome::Running { .. })
                    })
                {
                    return Err(format!(
                        "a backup is already running for target {}",
                        run.target_id
                    ));
                }
                self.restic_runs.insert(run.id.clone(), run.clone());
                let mut same_target: Vec<&BackupRun> = self
                    .restic_runs
                    .values()
                    .filter(|r| r.drive == run.drive && r.destination_id == run.destination_id)
                    .collect();
                if same_target.len() > MAX_BACKUP_RUNS_PER_TARGET {
                    // Oldest first, never the run just recorded (may carry an old timestamp); finished before still Running.
                    same_target.retain(|r| r.id != run.id);
                    same_target.sort_by_key(|r| {
                        (
                            matches!(r.outcome, BackupRunOutcome::Running { .. }),
                            r.started_at,
                        )
                    });
                    let drop_count =
                        (same_target.len() + 1 - MAX_BACKUP_RUNS_PER_TARGET).min(same_target.len());
                    let to_drop: Vec<String> = same_target
                        .iter()
                        .take(drop_count)
                        .map(|r| r.id.clone())
                        .collect();
                    for id in to_drop {
                        self.restic_runs.remove(&id);
                    }
                }
                Ok(())
            }

            Request::PutRestoreRequest(request) => {
                validate_object_name(&request.id)?;
                match &request.scope {
                    RestoreScope::Drive { drive }
                    | RestoreScope::Volume {
                        volume: VolumeId { drive, .. },
                    } => {
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
                let request_id = request.id.clone();
                self.restore_requests.insert(request_id.clone(), request);

                if self.restore_requests.len() > MAX_RESTORE_REQUESTS {
                    // Never the request just inserted (old timestamp possible); finished before Pending/Running.
                    let mut all: Vec<&RestoreRequest> = self
                        .restore_requests
                        .values()
                        .filter(|r| r.id != request_id)
                        .collect();
                    all.sort_by_key(|r| {
                        (
                            matches!(r.status, RestoreStatus::Pending | RestoreStatus::Running),
                            r.requested_at,
                        )
                    });
                    let drop_count = (all.len() + 1 - MAX_RESTORE_REQUESTS).min(all.len());
                    let to_drop: Vec<String> =
                        all.iter().take(drop_count).map(|r| r.id.clone()).collect();
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
                if matches!(
                    request.status,
                    RestoreStatus::Succeeded | RestoreStatus::Failed { .. }
                ) {
                    return Err(format!("restore request {id} already finished"));
                }
                if matches!(status, RestoreStatus::Running)
                    && !matches!(request.status, RestoreStatus::Pending)
                {
                    return Err(format!(
                        "restore request {id} is not pending, cannot be claimed"
                    ));
                }
                if matches!(
                    status,
                    RestoreStatus::Succeeded | RestoreStatus::Failed { .. }
                ) {
                    request.finished_at = Some(*at);
                }
                request.status = status.clone();
                Ok(())
            }

            Request::ReportContainerStatus {
                node,
                running,
                secret_revisions,
                config_revisions,
            } => {
                self.container_status
                    .retain(|_, status| &status.node != node);
                for (instance, is_running) in running {
                    self.container_status.insert(
                        instance.clone(),
                        ContainerStatus {
                            node: node.clone(),
                            running: *is_running,
                            secret_revisions: secret_revisions
                                .get(instance)
                                .cloned()
                                .unwrap_or_default(),
                            config_revisions: config_revisions
                                .get(instance)
                                .cloned()
                                .unwrap_or_default(),
                        },
                    );
                }
                Ok(())
            }

            Request::SetInstanceUpdate {
                instance,
                node,
                phase,
                error,
                at,
            } => {
                if !self.assignments.contains_key(instance) {
                    return Err(format!("unknown instance {instance}"));
                }
                // A phase change keeps its start time; a fresh update after a finished one restarts it.
                let started_at = match self.instance_updates.get(instance) {
                    Some(current) if !current.phase.is_terminal() => current.started_at,
                    _ => *at,
                };
                self.instance_updates.insert(
                    instance.clone(),
                    InstanceUpdate {
                        node: node.clone(),
                        phase: *phase,
                        started_at,
                        at: *at,
                        error: error.clone(),
                    },
                );
                Ok(())
            }

            Request::ClearInstanceUpdate { instance } => {
                self.instance_updates.remove(instance);
                Ok(())
            }

            Request::SetImageChecks { checks, at } => {
                let mut next = checks.clone();
                for (image, check) in next.iter_mut() {
                    if check.latest_digest.is_none() && check.error.is_some() {
                        check.latest_digest = self
                            .image_checks
                            .get(image)
                            .and_then(|old| old.latest_digest.clone());
                    }
                }
                self.image_checks = next;
                self.image_checks_at = *at;
                Ok(())
            }

            Request::SetAutoUpdate {
                instance,
                digest,
                outcome,
                error,
                at,
            } => {
                if !self.assignments.contains_key(instance) {
                    return Err(format!("unknown instance {instance}"));
                }
                let detected_at = match self.auto_updates.get(instance) {
                    Some(current) if &current.digest == digest => current.detected_at,
                    _ => *at,
                };
                self.auto_updates.insert(
                    instance.clone(),
                    AutoUpdateRecord {
                        digest: digest.clone(),
                        detected_at,
                        outcome: *outcome,
                        at: *at,
                        error: error.clone(),
                    },
                );
                Ok(())
            }

            Request::ClearErrors => {
                self.restic_runs
                    .retain(|_, r| !matches!(r.outcome, BackupRunOutcome::Failed { .. }));
                self.restore_requests
                    .retain(|_, r| !matches!(r.status, RestoreStatus::Failed { .. }));
                self.instance_updates
                    .retain(|_, u| !matches!(u.phase, UpdatePhase::Failed));
                for assignment in self.assignments.values_mut() {
                    if matches!(
                        assignment.migration,
                        Some(Migration {
                            phase: MigrationPhase::Failed { .. },
                            ..
                        })
                    ) {
                        assignment.migration = None;
                    }
                }
                // A still-failing reconciler rewrites its error next pass, so this only clears stale ones.
                for statuses in [&mut self.cert_status, &mut self.dns_status] {
                    statuses.retain(|_, s| !matches!(s, ReconcileStatus::Error { .. }));
                }
                self.alert_destinations.retain(|_, h| h.ok);
                Ok(())
            }

            Request::PutSession { key, session, now } => {
                self.sessions.retain(|_, s| s.expires_at > *now);
                self.sessions.insert(key.clone(), session.clone());
                Ok(())
            }

            Request::RemoveSession { key } => {
                self.sessions.remove(key);
                Ok(())
            }

            Request::PruneSessions { now } => {
                self.sessions.retain(|_, s| s.expires_at > *now);
                self.sweep_proxy_auth(*now);
                Ok(())
            }

            Request::PutProxySession { key, session, now } => {
                self.sweep_proxy_auth(*now);
                self.proxy_sessions.insert(key.clone(), session.clone());
                Ok(())
            }

            Request::RemoveProxySession { key } => {
                self.proxy_sessions.remove(key);
                self.proxy_sessions.retain(|_, s| &s.parent != key);
                self.proxy_tokens.retain(|_, t| &t.parent != key);
                Ok(())
            }

            Request::PutProxyToken { key, token, now } => {
                self.sweep_proxy_auth(*now);
                self.proxy_tokens.insert(key.clone(), token.clone());
                Ok(())
            }

            Request::RedeemProxyToken {
                token_key,
                host,
                session_key,
                now,
            } => {
                let Some(token) = self.proxy_tokens.get(token_key) else {
                    return Err("unknown or already used token".into());
                };
                if token.expires_at <= *now {
                    return Err("token expired".into());
                }
                if &token.host != host {
                    return Err("token was issued for another host".into());
                }
                if token.session_expires_at <= *now {
                    return Err("login expired".into());
                }
                if !token.parent.is_empty()
                    && self
                        .proxy_sessions
                        .get(&token.parent)
                        .is_none_or(|p| p.expires_at <= *now)
                {
                    return Err("signed out".into());
                }
                let Some(token) = self.proxy_tokens.remove(token_key) else {
                    return Err("unknown or already used token".into());
                };
                self.sweep_proxy_auth(*now);
                self.proxy_sessions.insert(
                    session_key.clone(),
                    ProxySessionRecord {
                        host: token.host,
                        subject: token.subject,
                        username: token.username,
                        groups: token.groups,
                        expires_at: token.session_expires_at,
                        parent: token.parent,
                    },
                );
                Ok(())
            }

            Request::PushAlertSignal(signal) => {
                self.alert_inbox.insert(signal.id.clone(), signal.clone());
                while self.alert_inbox.len() > MAX_ALERT_INBOX {
                    let oldest = self
                        .alert_inbox
                        .iter()
                        .min_by(|a, b| (a.1.at, a.0).cmp(&(b.1.at, b.0)))
                        .map(|(id, _)| id.clone());
                    match oldest {
                        Some(id) => self.alert_inbox.remove(&id),
                        None => break,
                    };
                }
                Ok(())
            }

            Request::TakeAlertSignals { ids } => {
                for id in ids {
                    self.alert_inbox.remove(id);
                }
                Ok(())
            }

            Request::PutAlert(record) => {
                let mut record = record.clone();
                let excess = record
                    .deliveries
                    .len()
                    .saturating_sub(MAX_RECORD_DELIVERIES);
                record.deliveries.drain(..excess);
                self.alerts.insert(record.id.clone(), record);
                self.trim_alerts(MAX_ALERT_RECORDS);
                Ok(())
            }

            Request::RemoveAlert { id } => {
                self.alerts.remove(id);
                Ok(())
            }

            Request::PruneAlerts { before, max } => {
                self.alerts
                    .retain(|_, a| a.state.is_open() || a.last_at >= *before);
                self.trim_alerts(*max);
                Ok(())
            }

            Request::SetDestinationHealth { name, health } => {
                self.alert_destinations.insert(name.clone(), health.clone());
                Ok(())
            }

            Request::RemoveDestinationHealth { name } => {
                self.alert_destinations.remove(name);
                Ok(())
            }

            Request::SetStackTerraform { .. }
            | Request::PutTerraformState { .. }
            | Request::RecordTerraformRun { .. }
            | Request::PutTerraformOutputs { .. }
            | Request::RequestTerraformRun { .. } => self.apply_terraform(req),

            Request::Served { drive, op } => {
                let next = crate::served::ServedAuthority::apply(
                    self.served.get(drive),
                    op,
                    &self.unreachable,
                )?;
                self.served.insert(drive.clone(), next);
                Ok(())
            }

            Request::RecordCrashMove { instance, from, at } => {
                if !self.assignments.contains_key(instance) {
                    return Err(format!("unknown instance {instance}"));
                }
                let moves = self.crash_moves.entry(instance.clone()).or_default();
                moves.retain(|_, t| at.saturating_sub(*t) <= CRASH_MOVE_RETENTION_SECS);
                moves.insert(from.clone(), *at);
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

            Request::ClearReconcileStatus { fqdn } => {
                self.cert_status.remove(fqdn);
                self.dns_status.remove(fqdn);
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

    /// Whether a node may take new work: not held back by the operator and still heard from.
    pub fn can_schedule(&self, node: &str) -> bool {
        self.nodes.get(node).is_some_and(|n| n.schedulable()) && !self.unreachable.contains(node)
    }

    /// The core scheduling invariant: an instance only lands on a node that takes
    /// work and implements every *replicated* drive its volumes live on. A served
    /// drive has one authoritative copy reachable by any node, so it never narrows
    /// candidates; this mirrors the carve-out in `cl_sched::place::rejection` and
    /// the two must agree or a scheduler-chosen served-drive placement would be rejected here.
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

    /// Drive names whose mode is `served`, parsed from the replicated config's raw
    /// yaml with a minimal local shape (see `ClusterConfigEntry`). Empty if no
    /// config is published or it fails to parse, like `cl_sched::run::served_drives`.
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
        shape
            .drives
            .into_iter()
            .filter(|(_, d)| d.mode == "served")
            .map(|(name, _)| name)
            .collect()
    }

    /// Stacks referencing a config, so removing one in use is refused.
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

    /// Whether `drive` is declared in the replicated config at all (served or replicated).
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

    /// Backup targets referencing a destination, so removing one in use is refused.
    fn destination_in_use(&self, id: &str) -> Option<String> {
        self.restic_targets
            .values()
            .find(|t| t.destination_id == id)
            .map(|t| format!("backup target {}", t.id))
    }

    /// A copy with every secret blanked, for anything that leaves the node; lengths stay right so the ui can show a secret is set.
    pub fn redacted(&self) -> ClusterState {
        let mut copy = self.clone();
        for secret in copy.secrets.values_mut() {
            secret.sealed = String::new();
        }
        for tf in copy.terraform_states.values_mut() {
            tf.sealed = String::new();
        }
        for cert in copy.certificates.values_mut() {
            cert.key_pem = String::new();
        }
        // Login material, including saved client addresses: the ui never needs it and
        // the admin list reads the unredacted state.
        copy.proxy_sessions.clear();
        copy.proxy_tokens.clear();
        copy
    }

    /// A stack's priority for the scheduler and drive syncs: lower is more important; prioritized beats all.
    pub fn effective_priority(&self, stack: &str, own: i32) -> i32 {
        if self.prioritized.contains(stack) {
            i32::MIN
        } else {
            own
        }
    }

    pub fn in_emergency(&self) -> bool {
        !self.emergency_stacks.is_empty()
    }

    /// Whether `stack` may be scheduled: always outside emergency mode, only emergency stacks during it.
    pub fn emergency_allows(&self, stack: &str) -> bool {
        self.emergency_stacks.is_empty() || self.emergency_stacks.contains(stack)
    }

    pub fn assignments_on<'a>(&'a self, node: &'a str) -> impl Iterator<Item = &'a Assignment> {
        self.assignments.values().filter(move |a| a.node == node)
    }
}

/// Base64 helpers, so the attachment encoding is defined in one place.
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

pub fn dns_key(fqdn: &str, record_type: &str, content: &str) -> String {
    format!("{fqdn}|{record_type}|{content}")
}

/// Stacks whose spec mounts the volume (or the whole drive), whether or not an instance is assigned.
pub fn stacks_declaring_volume(state: &ClusterState, id: &crate::ids::VolumeId) -> Vec<String> {
    use cl_config::StackFile;

    state
        .stacks
        .iter()
        .filter(|(_, stack)| {
            StackFile::parse(&stack.spec).is_ok_and(|file| {
                file.services.values().any(|svc| {
                    svc.volumes.iter().any(|m| {
                        m.drive.as_deref() == Some(id.drive.as_str())
                            && matches!(m.volume.as_deref(), Some(v) if v == id.volume || v == ".")
                    })
                })
            })
        })
        .map(|(name, _)| name.clone())
        .collect()
}

pub fn instances_using_volume(
    state: &ClusterState,
    id: &crate::ids::VolumeId,
) -> Vec<crate::ids::InstanceId> {
    use cl_config::StackFile;

    let mut found = Vec::new();
    for assignment in state.assignments.values() {
        let instance = &assignment.instance;
        let Some(stack) = state.stacks.get(&instance.stack) else {
            continue;
        };
        let Ok(file) = StackFile::parse(&stack.spec) else {
            continue;
        };
        let Some(service) = file.services.get(&instance.service) else {
            continue;
        };
        let uses = service.volumes.iter().any(|m| {
            m.drive.as_deref() == Some(id.drive.as_str())
                && m.volume.as_deref() == Some(id.volume.as_str())
        });
        if uses {
            found.push(instance.clone());
        }
    }
    found
}

/// Instances that must be down while `volumes` is overwritten or edited: those
/// mounting one of them, plus those mounting the whole drive root (`.`) of the
/// same drive, which contains every volume on it.
pub fn instances_touching_volumes(
    state: &ClusterState,
    volumes: &[crate::ids::VolumeId],
) -> Vec<crate::ids::InstanceId> {
    let mut found: BTreeSet<crate::ids::InstanceId> = BTreeSet::new();
    for volume in volumes {
        found.extend(instances_using_volume(state, volume));
        if volume.volume != "." {
            found.extend(instances_using_volume(
                state,
                &crate::ids::VolumeId::new(volume.drive.clone(), ".".to_string()),
            ));
        }
    }
    found.into_iter().collect()
}

impl ClusterState {
    /// Running instances on nodes other than `node` that use `volume`
    /// (directly or through a whole-drive mount): what a freeze on `node`
    /// has to stop.
    pub fn running_elsewhere(
        &self,
        node: &str,
        volume: &crate::ids::VolumeId,
    ) -> Vec<crate::ids::InstanceId> {
        instances_touching_volumes(self, std::slice::from_ref(volume))
            .into_iter()
            .filter(|i| {
                self.assignments
                    .get(i)
                    .is_some_and(|a| a.desired == DesiredState::Running && a.node != node)
            })
            .collect()
    }

    /// Reachable nodes implementing the volume's drive whose copy is behind its current generation.
    pub fn replicas_behind(&self, volume: &crate::ids::VolumeId) -> Vec<String> {
        let Some(v) = self.volumes.get(volume) else {
            return Vec::new();
        };
        self.nodes
            .iter()
            .filter(|(name, n)| n.implements(&volume.drive) && !self.unreachable.contains(*name))
            .filter(|(name, _)| v.replicas.get(*name).copied().unwrap_or(0) < v.generation)
            .map(|(name, _)| name.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::alerts::{AlertState, SignalAction};
    use cl_config::AlertSeverity;

    fn sample_signal(id: &str) -> AlertSignal {
        AlertSignal {
            id: id.into(),
            event: "node.disk_low".into(),
            dedupe: "node.disk_low:n1:/data".into(),
            action: SignalAction::Open,
            severity: Some(AlertSeverity::Warning),
            title: "t".into(),
            detail: String::new(),
            subject: "n1".into(),
            stack: None,
            instance: None,
            node: Some("n1".into()),
            at: 1,
            excerpt: Vec::new(),
            count: 1,
        }
    }

    fn sample_alert(id: &str, state: AlertState, last_at: i64) -> AlertRecord {
        AlertRecord {
            id: id.into(),
            event: "node.disk_low".into(),
            dedupe: format!("d-{id}"),
            category: "nodes".into(),
            severity: AlertSeverity::Warning,
            state,
            title: "t".into(),
            detail: String::new(),
            subject: "n1".into(),
            stack: None,
            instance: None,
            node: None,
            opened_at: last_at,
            last_at,
            resolved_at: None,
            count: 1,
            escalated_from: None,
            sent_severity: None,
            resolved_sent: false,
            digested: false,
            links: Vec::new(),
            excerpt: Vec::new(),
            deliveries: Vec::new(),
        }
    }

    #[test]
    fn alert_inbox_is_bounded_and_drops_the_oldest() {
        let mut state = ClusterState::default();
        for i in 0..(MAX_ALERT_INBOX + 5) {
            let mut sig = sample_signal(&format!("s{i:04}"));
            sig.at = i as i64;
            assert!(state.apply(&Request::PushAlertSignal(sig)).is_ok());
        }
        assert_eq!(state.alert_inbox.len(), MAX_ALERT_INBOX);
        assert!(!state.alert_inbox.contains_key("s0000"));
        assert!(state.alert_inbox.contains_key("s0504"));
        state.apply(&Request::TakeAlertSignals {
            ids: vec!["s0504".into(), "nope".into()],
        });
        assert_eq!(state.alert_inbox.len(), MAX_ALERT_INBOX - 1);
    }

    #[test]
    fn pruning_alerts_keeps_open_ones_and_trims_oldest_resolved_first() {
        let mut state = ClusterState::default();
        state.apply(&Request::PutAlert(sample_alert(
            "open",
            AlertState::Active,
            1,
        )));
        state.apply(&Request::PutAlert(sample_alert(
            "old",
            AlertState::Resolved,
            2,
        )));
        state.apply(&Request::PutAlert(sample_alert(
            "mid",
            AlertState::Event,
            50,
        )));
        state.apply(&Request::PutAlert(sample_alert(
            "new",
            AlertState::Resolved,
            90,
        )));
        state.apply(&Request::PruneAlerts {
            before: 10,
            max: 10,
        });
        assert!(state.alerts.contains_key("open"));
        assert!(!state.alerts.contains_key("old"));
        state.apply(&Request::PruneAlerts { before: 0, max: 2 });
        assert_eq!(
            state.alerts.keys().cloned().collect::<Vec<_>>(),
            vec!["new".to_string(), "open".to_string()]
        );
        state.apply(&Request::RemoveAlert { id: "new".into() });
        assert_eq!(state.alerts.len(), 1);
    }

    #[test]
    fn put_alert_caps_the_delivery_log() {
        let mut state = ClusterState::default();
        let mut rec = sample_alert("a", AlertState::Active, 1);
        for i in 0..30 {
            rec.deliveries.push(crate::alerts::AlertDelivery {
                destination: "d".into(),
                at: i,
                ok: true,
                error: None,
                attempts: 1,
                phase: "opened".into(),
                message_id: None,
            });
        }
        state.apply(&Request::PutAlert(rec));
        let kept = &state.alerts["a"].deliveries;
        assert_eq!(kept.len(), MAX_RECORD_DELIVERIES);
        assert_eq!(kept.last().unwrap().at, 29);
    }
    use super::*;

    #[test]
    fn backup_run_outcome_reads_the_pre_progress_unit_variant() {
        let legacy: BackupRunOutcome = serde_json::from_str("\"Running\"").unwrap();
        assert_eq!(legacy, BackupRunOutcome::Running { percent: None });
    }

    #[test]
    fn backup_run_outcome_round_trips_the_current_shape() {
        for outcome in [
            BackupRunOutcome::Running {
                percent: Some(0.42),
            },
            BackupRunOutcome::Succeeded {
                snapshot_id: "abc".into(),
                bytes: 7,
                warning: None,
            },
            BackupRunOutcome::Failed {
                error: "boom".into(),
            },
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
        let req: Request = serde_json::from_value(from_a_newer_node.clone()).unwrap();
        assert_eq!(req, Request::Unknown(from_a_newer_node.clone()));
        // Re-serializing reproduces the payload verbatim, so an old leader
        // re-replicates it losslessly.
        assert_eq!(serde_json::to_value(&req).unwrap(), from_a_newer_node);

        let mut state = ClusterState::default();
        let before = state.version;
        let response = state.apply(&req);
        assert!(response.is_ok(), "{:?}", response.error);
        assert_eq!(state.version, before, "Unknown must not bump version");
    }

    #[test]
    fn a_known_request_tag_with_a_bad_body_is_a_hard_error() {
        let bad = serde_json::json!({ "PutClusterConfig": { "yaml": 5 } });
        assert!(serde_json::from_value::<Request>(bad).is_err());
        // Unit variant tag used as an object, and non-tag-shaped payloads.
        assert!(serde_json::from_value::<Request>(serde_json::json!({"NoOp": {"x": 1}})).is_err());
        assert!(serde_json::from_value::<Request>(serde_json::json!(7)).is_err());
        assert!(
            serde_json::from_value::<Request>(serde_json::json!({"NoOp": null, "b": 1})).is_err()
        );
    }

    #[test]
    fn a_legacy_unit_unknown_string_still_deserializes_and_round_trips() {
        let req: Request = serde_json::from_str("\"Unknown\"").unwrap();
        assert_eq!(req, Request::Unknown(serde_json::json!("Unknown")));
        assert_eq!(serde_json::to_string(&req).unwrap(), "\"Unknown\"");
    }

    #[test]
    fn every_current_request_variant_round_trips_and_is_not_unknown() {
        // One instance per real `Request` variant (not `Unknown`); guards against
        // `request_mirror::RequestMirror` drifting, which would deserialize a variant as `Unknown`.
        let requests = vec![
            Request::PutClusterConfig {
                yaml: "y".into(),
                expect_generation: 0,
            },
            Request::UpsertNode(node("n1", &["fast"])),
            Request::SetAvailability {
                node: "n1".into(),
                availability: Availability::Drain,
            },
            Request::SetReachable {
                node: "n1".into(),
                reachable: false,
            },
            Request::RemoveNode { node: "n1".into() },
            Request::RetireNodeId {
                id: uuid::Uuid::new_v4(),
            },
            Request::DeployStack {
                name: "s".into(),
                spec: "spec".into(),
                at: 0,
            },
            Request::RemoveStack { name: "s".into() },
            Request::SetStackObjects {
                name: "s".into(),
                configs: drives(&["c1"]),
                secrets: drives(&["sec1"]),
                used_configs: drives(&["c1"]),
                used_secrets: drives(&["sec1"]),
            },
            Request::Assign {
                instance: InstanceId::new("s", "web", 0),
                node: "n1".into(),
                drives: drives(&["fast"]),
            },
            Request::Unassign {
                instance: InstanceId::new("s", "web", 0),
            },
            Request::SetDesiredState {
                instance: InstanceId::new("s", "web", 0),
                desired: DesiredState::Stopped,
            },
            Request::SetImageDigest {
                instance: InstanceId::new("s", "web", 0),
                digest: "sha256:abc".into(),
            },
            Request::BeginMigration {
                instance: InstanceId::new("s", "web", 0),
                to: "n2".into(),
                drives: drives(&["fast"]),
            },
            Request::AdvanceMigration {
                instance: InstanceId::new("s", "web", 0),
                phase: MigrationPhase::Syncing,
            },
            Request::ClearMigration {
                instance: InstanceId::new("s", "web", 0),
            },
            Request::UpsertVolume {
                id: VolumeId::new("fast", "v1"),
                primary: "n1".into(),
            },
            Request::SetVolumeReplica {
                id: VolumeId::new("fast", "v1"),
                node: "n1".into(),
                generation: 2,
            },
            Request::TouchVolume {
                id: VolumeId::new("fast", "v1"),
                primary: "n1".into(),
            },
            Request::SetVolumeSyncRefusal {
                id: VolumeId::new("fast", "v1"),
                refusal: None,
            },
            Request::SetVolumeEmpty {
                id: VolumeId::new("fast", "v1"),
                empty: true,
            },
            Request::RemoveVolume {
                id: VolumeId::new("fast", "v1"),
            },
            Request::AckVolumeRemoved {
                id: VolumeId::new("fast", "v1"),
                node: "n1".into(),
            },
            Request::FreezeVolume {
                id: VolumeId::new("fast", "v1"),
                node: "n1".into(),
                stopped: vec![],
            },
            Request::UnfreezeVolume {
                id: VolumeId::new("fast", "v1"),
            },
            Request::StoreCertificate(CertificateState {
                fqdn: "example.com".into(),
                chain_pem: "chain".into(),
                key_pem: "key".into(),
                not_after: 0,
                issued_at: 0,
            }),
            Request::RemoveCertificate {
                fqdn: "example.com".into(),
            },
            Request::SetCertStatus {
                fqdn: "example.com".into(),
                status: ReconcileStatus::Pending,
            },
            Request::UpsertDnsRecord(DnsRecordState {
                fqdn: "example.com".into(),
                record_type: "A".into(),
                content: "1.2.3.4".into(),
                zone: "example.com".into(),
                provider_id: None,
            }),
            Request::RemoveDnsRecord {
                key: "example.com".into(),
            },
            Request::SetDnsStatus {
                fqdn: "example.com".into(),
                status: ReconcileStatus::Pending,
            },
            Request::ClearReconcileStatus {
                fqdn: "example.com".into(),
            },
            Request::PutConfigCas {
                name: "c".into(),
                content: base64_encode(b"x"),
                owner: None,
                expect_revision: 1,
            },
            Request::PutConfig {
                name: "c1".into(),
                content: "aGk=".into(),
                owner: None,
            },
            Request::RemoveConfig { name: "c1".into() },
            Request::PutSecret {
                name: "sec1".into(),
                sealed: "sealed".into(),
                key_fingerprint: "fp".into(),
                owner: None,
            },
            Request::RemoveSecret {
                name: "sec1".into(),
            },
            Request::PutWrappedKey {
                node: "n1".into(),
                entry: WrappedKeyEntry {
                    ephemeral_public: "e".into(),
                    sealed: "s".into(),
                    key_fingerprint: "fp".into(),
                    target_pubkey: "t".into(),
                },
            },
            Request::SetOverlayAllocations {
                allocations: "{}".into(),
            },
            Request::PutBackupDestination(BackupDestination {
                id: "d1".into(),
                name: "d1".into(),
                kind: BackupDestinationKind::NodeLocal {
                    node: "n1".into(),
                    path: "/backups".into(),
                },
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
                keep_daily: None,
                keep_weekly: None,
                keep_monthly: None,
                keep_yearly: None,
                stop_containers: None,
                last_run: None,
            }),
            Request::RemoveBackupTarget { id: "t1".into() },
            Request::SetBackupTargetLastRun {
                id: "t1".into(),
                at: 0,
            },
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
                scope: RestoreScope::Volume {
                    volume: VolumeId::new("fast", "v1"),
                },
                destination_id: None,
                snapshot: None,
                requested_at: 0,
                finished_at: None,
                status: RestoreStatus::Pending,
                requested_by: "op".into(),
            }),
            Request::SetRestoreStatus {
                id: "res1".into(),
                status: RestoreStatus::Pending,
                at: 0,
            },
            Request::ReportContainerStatus {
                node: "n1".into(),
                running: BTreeMap::from([(InstanceId::new("gitlab", "web", 0), true)]),
                secret_revisions: BTreeMap::new(),
                config_revisions: BTreeMap::new(),
            },
            Request::SetInstanceUpdate {
                instance: InstanceId::new("gitlab", "web", 0),
                node: "n1".into(),
                phase: UpdatePhase::Failed,
                error: Some("pull failed".into()),
                at: 5,
            },
            Request::ClearInstanceUpdate {
                instance: InstanceId::new("gitlab", "web", 0),
            },
            Request::SetAutoUpdate {
                instance: InstanceId::new("gitlab", "web", 0),
                digest: "sha256:a".into(),
                outcome: AutoUpdateOutcome::Waiting,
                error: None,
                at: 5,
            },
            Request::SetImageChecks {
                checks: BTreeMap::from([(
                    "nginx:1".to_string(),
                    ImageCheck {
                        latest_digest: Some("sha256:a".into()),
                        checked_at: 5,
                        error: None,
                    },
                )]),
                at: 5,
            },
            Request::ClearErrors,
            Request::PutSession {
                key: "k".into(),
                session: SessionRecord {
                    subject: "s".into(),
                    username: "u".into(),
                    admin: false,
                    expires_at: 10,
                },
                now: 1,
            },
            Request::RemoveSession { key: "k".into() },
            Request::PruneSessions { now: 5 },
            Request::PushAlertSignal(sample_signal("s1")),
            Request::TakeAlertSignals {
                ids: vec!["s1".into()],
            },
            Request::PutAlert(sample_alert("a1", AlertState::Active, 10)),
            Request::RemoveAlert { id: "a1".into() },
            Request::PruneAlerts { before: 5, max: 10 },
            Request::SetDestinationHealth {
                name: "discord".into(),
                health: DestinationHealth {
                    ok: false,
                    at: 3,
                    error: Some("boom".into()),
                    consecutive_failures: 2,
                },
            },
            Request::RemoveDestinationHealth {
                name: "discord".into(),
            },
            Request::PutProxySession {
                key: "k".into(),
                session: ProxySessionRecord {
                    host: "a.example.com".into(),
                    subject: "s".into(),
                    username: "u".into(),
                    groups: vec!["g".into()],
                    expires_at: 10,
                    parent: String::new(),
                },
                now: 1,
            },
            Request::RemoveProxySession { key: "k".into() },
            Request::PutProxyToken {
                key: "t".into(),
                token: ProxyTokenRecord {
                    host: "a.example.com".into(),
                    subject: "s".into(),
                    username: "u".into(),
                    groups: Vec::new(),
                    session_expires_at: 10,
                    expires_at: 5,
                    parent: String::new(),
                },
                now: 1,
            },
            Request::RedeemProxyToken {
                token_key: "t".into(),
                host: "a.example.com".into(),
                session_key: "k".into(),
                now: 2,
            },
            Request::RecordCrashMove {
                instance: InstanceId::new("gitlab", "web", 0),
                from: "n1".into(),
                at: 5,
            },
            Request::NoOp,
            Request::ClearEmergency,
            Request::SetPrioritized {
                stack: "s".into(),
                on: true,
            },
            Request::Served {
                drive: "fast".into(),
                op: crate::served::ServedOp::RequestFailback,
            },
            Request::SetStackTerraform {
                stack: "s".into(),
                modules: BTreeMap::from([("web".to_string(), "x".to_string())]),
            },
            Request::PutTerraformState {
                key: "s/web".into(),
                sealed: "c2VhbGVk".into(),
                key_fingerprint: "fp".into(),
                expect_serial: 3,
            },
            Request::RecordTerraformRun {
                run: crate::terraform::TerraformRun {
                    id: "r1".into(),
                    mode: crate::terraform::TerraformMode::Apply,
                    trigger: "change".into(),
                    node: "n1".into(),
                    instance: InstanceId::new("s", "web", 0),
                    fingerprint: "f".into(),
                    started_at: 1,
                    finished_at: Some(2),
                    outcome: crate::terraform::TerraformOutcome::Failed { error: "e".into() },
                    log_tail: "l".into(),
                },
            },
            Request::PutTerraformOutputs {
                key: "s/web".into(),
                run_id: "r1".into(),
                secrets: vec![crate::terraform::TerraformSecretOutput {
                    name: "sec".into(),
                    sealed: "c2VhbGVk".into(),
                    key_fingerprint: "fp".into(),
                }],
                configs: vec![crate::terraform::TerraformConfigOutput {
                    name: "cfg".into(),
                    content: "Zm9v".into(),
                }],
                claims: vec![crate::terraform::OutputObject::config("c2")],
            },
            Request::RequestTerraformRun {
                key: "s/web".into(),
                mode: crate::terraform::TerraformMode::Plan,
            },
        ];

        for req in requests {
            let json = serde_json::to_string(&req).unwrap();
            let back: Request = serde_json::from_str(&json).unwrap();
            assert!(
                !matches!(back, Request::Unknown(_)),
                "variant round-tripped as Unknown: {json}"
            );
            let value = serde_json::to_value(&req).unwrap();
            let tag = match &value {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Object(m) => m.keys().next().unwrap().clone(),
                other => panic!("untagged request shape {other}"),
            };
            assert!(
                KNOWN_REQUEST_VARIANTS.contains(&tag.as_str()),
                "{tag} missing from KNOWN_REQUEST_VARIANTS"
            );
            assert_eq!(back, req);
        }
    }

    #[test]
    fn cluster_state_ignores_an_unknown_top_level_field() {
        // Guards against `#[serde(deny_unknown_fields)]`: a new field from a newer node must stay ignorable by an older one.
        let mut value = serde_json::to_value(ClusterState::default()).unwrap();
        value.as_object_mut().unwrap().insert(
            "some_field_from_the_future".into(),
            serde_json::json!({"anything": true}),
        );

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

    #[test]
    fn holds_image_matches_a_pinned_digest_or_a_tag() {
        let mut n = node("n1", &[]);
        n.images.insert("nginx:1.25".into());
        n.images.insert("ghcr.io/o/app@sha256:abc".into());
        assert!(n.holds_image("nginx:1.25", None));
        assert!(!n.holds_image("nginx:1.26", None));
        assert!(n.holds_image("ghcr.io/o/app:v2", Some("sha256:abc")));
        assert!(!n.holds_image("ghcr.io/o/app:v2", Some("sha256:def")));
        assert!(!node("n2", &[]).holds_image("nginx:1.25", None));
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
            last_resort: false,
            egress_networks: Default::default(),
            networks: Default::default(),
            open_ports: Default::default(),
            arch: Default::default(),
            binary_hash: Default::default(),
            node_pubkey: Default::default(),
            memory_total_bytes: Default::default(),
            gpus: Default::default(),
            images: Default::default(),
            version: Default::default(),
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
    fn put_config_cas_rejects_a_stale_revision() {
        let mut state = cluster();
        let put = |state: &mut ClusterState, body: &[u8], expect: u64| {
            state.apply(&Request::PutConfigCas {
                name: "run".into(),
                content: base64_encode(body),
                owner: None,
                expect_revision: expect,
            })
        };
        assert!(put(&mut state, b"one", 0).is_ok());
        assert_eq!(state.configs["run"].revision, 1);
        // A writer that read revision 1 wins; one that also read 1 loses.
        assert!(put(&mut state, b"two", 1).is_ok());
        let stale = put(&mut state, b"three", 1);
        assert!(!stale.is_ok());
        assert_eq!(state.configs["run"].decode().expect("decode"), b"two");
        // Creating over an existing object is a conflict too.
        assert!(!put(&mut state, b"four", 0).is_ok());
    }

    #[test]
    fn repinning_remembers_the_previous_digest_unless_it_is_unchanged() {
        let mut state = cluster();
        assert!(assign(&mut state, "n1", &[]).is_ok());
        let instance = InstanceId::new("gitlab", "web", 0);
        let pin = |state: &mut ClusterState, digest: &str| {
            state.apply(&Request::SetImageDigest {
                instance: instance.clone(),
                digest: digest.to_string(),
            })
        };
        pin(&mut state, "sha256:a");
        assert!(!state.previous_image_digests.contains_key(&instance));
        pin(&mut state, "sha256:b");
        assert_eq!(state.previous_image_digests[&instance], "sha256:a");
        pin(&mut state, "sha256:b");
        assert_eq!(state.previous_image_digests[&instance], "sha256:a");
        state.apply(&Request::Unassign {
            instance: instance.clone(),
        });
        assert!(state.previous_image_digests.is_empty());
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
        assert_eq!(
            state.assignments[&instance].image_digest.as_deref(),
            Some("sha256:pinned")
        );

        // The scheduler moves the instance to a different node (its old one
        // went unreachable, say) by submitting `Assign` again, with no
        // intervening `Unassign`. The pin must survive that move.
        assert!(assign(&mut state, "n2", &[]).is_ok());
        assert_eq!(state.assignments[&instance].node, "n2");
        assert_eq!(
            state.assignments[&instance].image_digest.as_deref(),
            Some("sha256:pinned")
        );
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

    /// A served drive has one authoritative copy reachable by any node, so
    /// `check_placement` must not reject a node lacking a local directory for it;
    /// must agree with `cl_sched::place::rejection`.
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
        assert!(state
            .apply(&Request::RemoveNode { node: "n1".into() })
            .is_ok());
        assert!(!state.nodes.contains_key("n1"));
    }

    #[test]
    fn redeploying_a_stack_bumps_the_revision() {
        let mut state = ClusterState::default();
        state.apply(&Request::DeployStack {
            name: "gitlab".into(),
            spec: "a".into(),
            at: 0,
        });
        assert_eq!(state.stacks["gitlab"].revision, 1);
        state.apply(&Request::DeployStack {
            name: "gitlab".into(),
            spec: "b".into(),
            at: 0,
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
    fn clear_migration_only_clears_a_failed_record() {
        let mut state = cluster();
        let instance = InstanceId::new("gitlab", "web", 0);
        assign(&mut state, "n1", &["fast"]);
        let clear = Request::ClearMigration {
            instance: instance.clone(),
        };

        // Nothing to clear.
        assert!(state
            .apply(&clear)
            .error
            .unwrap()
            .contains("no failed move"));

        state.apply(&Request::BeginMigration {
            instance: instance.clone(),
            to: "n2".into(),
            drives: drives(&["fast"]),
        });
        // Active move is refused.
        assert!(state
            .apply(&clear)
            .error
            .unwrap()
            .contains("still migrating"));
        assert!(state.assignments[&instance].migration.is_some());

        state.apply(&Request::AdvanceMigration {
            instance: instance.clone(),
            phase: MigrationPhase::Failed {
                reason: "boom".into(),
            },
        });
        assert!(state.apply(&clear).is_ok());
        assert!(state.assignments[&instance].migration.is_none());
        assert_eq!(state.assignments[&instance].node, "n1");
    }

    #[test]
    fn a_proxy_token_is_redeemed_once_for_its_own_host_only() {
        let mut state = ClusterState::default();
        let put = |state: &mut ClusterState, expires_at| {
            state.apply(&Request::PutProxyToken {
                key: "t".into(),
                token: ProxyTokenRecord {
                    host: "a.example.com".into(),
                    subject: "sub".into(),
                    username: "alice".into(),
                    groups: vec!["staff".into()],
                    session_expires_at: 1000,
                    expires_at,
                    parent: String::new(),
                },
                now: 0,
            });
        };
        let redeem = |state: &mut ClusterState, host: &str, key: &str, now| {
            state.apply(&Request::RedeemProxyToken {
                token_key: "t".into(),
                host: host.into(),
                session_key: key.into(),
                now,
            })
        };

        put(&mut state, 60);
        assert!(redeem(&mut state, "b.example.com", "s1", 1).error.is_some());
        assert!(state.proxy_sessions.is_empty());
        assert!(state.proxy_tokens.contains_key("t"));

        assert!(redeem(&mut state, "a.example.com", "s1", 1).error.is_none());
        let session = &state.proxy_sessions["s1"];
        assert_eq!(session.host, "a.example.com");
        assert_eq!(session.groups, vec!["staff"]);
        assert_eq!(session.expires_at, 1000);
        // Spent: a second redeem fails and mints nothing.
        assert!(redeem(&mut state, "a.example.com", "s2", 2).error.is_some());
        assert!(!state.proxy_sessions.contains_key("s2"));

        // Expired tokens do not redeem.
        put(&mut state, 60);
        assert!(redeem(&mut state, "a.example.com", "s3", 60)
            .error
            .is_some());
        assert!(!state.proxy_sessions.contains_key("s3"));
    }

    #[test]
    fn signing_out_ends_the_app_sessions_it_handed_out() {
        let mut state = ClusterState::default();
        let portal = ProxySessionRecord {
            host: "login.example.com".into(),
            subject: "sub".into(),
            username: "alice".into(),
            groups: Vec::new(),
            expires_at: 1000,
            parent: String::new(),
        };
        state.apply(&Request::PutProxySession {
            key: "portal".into(),
            session: portal,
            now: 0,
        });
        let token = |parent: &str| ProxyTokenRecord {
            host: "a.example.com".into(),
            subject: "sub".into(),
            username: "alice".into(),
            groups: Vec::new(),
            session_expires_at: 1000,
            expires_at: 60,
            parent: parent.into(),
        };
        for key in ["t1", "t2"] {
            state.apply(&Request::PutProxyToken {
                key: key.into(),
                token: token("portal"),
                now: 0,
            });
        }
        let redeem = |state: &mut ClusterState, t: &str, s: &str| {
            state.apply(&Request::RedeemProxyToken {
                token_key: t.into(),
                host: "a.example.com".into(),
                session_key: s.into(),
                now: 1,
            })
        };
        assert!(redeem(&mut state, "t1", "app").error.is_none());
        assert_eq!(state.proxy_sessions["app"].parent, "portal");

        state.apply(&Request::RemoveProxySession {
            key: "portal".into(),
        });
        assert!(state.proxy_sessions.is_empty());
        // A token minted before the sign-out is gone too, and a late one
        // naming the dead portal session cannot mint anything.
        assert!(state.proxy_tokens.is_empty());
        state.apply(&Request::PutProxyToken {
            key: "t3".into(),
            token: token("portal"),
            now: 2,
        });
        assert!(redeem(&mut state, "t3", "late").error.is_some());
        assert!(state.proxy_sessions.is_empty());
    }

    #[test]
    fn sessions_are_stored_removed_and_pruned() {
        let mut state = ClusterState::default();
        let session = |expires_at| SessionRecord {
            subject: "s".into(),
            username: "u".into(),
            admin: true,
            expires_at,
        };
        state.apply(&Request::PutSession {
            key: "old".into(),
            session: session(10),
            now: 0,
        });
        state.apply(&Request::PutSession {
            key: "live".into(),
            session: session(100),
            now: 50,
        });
        // Storing a new session sweeps the one that expired at 10.
        assert!(!state.sessions.contains_key("old"));
        assert!(state.sessions.contains_key("live"));

        state.apply(&Request::PruneSessions { now: 100 });
        assert!(state.sessions.is_empty());

        state.apply(&Request::PutSession {
            key: "k".into(),
            session: session(100),
            now: 0,
        });
        state.apply(&Request::RemoveSession { key: "k".into() });
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn prioritizing_needs_a_deployed_stack_and_goes_away_with_it() {
        let mut state = cluster();
        let on = |stack: &str| Request::SetPrioritized {
            stack: stack.into(),
            on: true,
        };
        assert!(state.apply(&on("site")).error.is_some());
        state.apply(&Request::DeployStack {
            name: "site".into(),
            spec: "services:\n  web:\n    image: nginx\n".into(),
            at: 0,
        });
        assert!(state.apply(&on("site")).error.is_none());
        assert_eq!(state.effective_priority("site", 5), i32::MIN);
        assert_eq!(state.effective_priority("other", 5), 5);
        state.apply(&Request::SetPrioritized {
            stack: "site".into(),
            on: false,
        });
        assert!(state.prioritized.is_empty());
        state.apply(&on("site"));
        state.apply(&Request::RemoveStack {
            name: "site".into(),
        });
        assert!(state.prioritized.is_empty());
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
            state.assignments[&instance]
                .migration
                .as_ref()
                .unwrap()
                .phase,
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
            state.assignments[&instance]
                .migration
                .as_ref()
                .unwrap()
                .from,
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
        assert!(rejected
            .error
            .unwrap()
            .contains("does not implement drive bulk"));
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
    fn a_sync_refusal_is_recorded_and_cleared() {
        let mut state = cluster();
        let id = VolumeId::new("fast", "data");
        state.apply(&Request::UpsertVolume {
            id: id.clone(),
            primary: "n1".into(),
        });
        let refusal = SyncRefusal {
            from: "n1".into(),
            to: "n2".into(),
            reason: "source is empty".into(),
            at: 100,
        };
        let set = Request::SetVolumeSyncRefusal {
            id: id.clone(),
            refusal: Some(refusal.clone()),
        };
        let first = state.apply(&set);
        assert!(first.is_ok());
        assert_eq!(state.volumes[&id].sync_refused, Some(refusal));

        state.apply(&Request::SetVolumeSyncRefusal {
            id: id.clone(),
            refusal: None,
        });
        assert!(state.volumes[&id].sync_refused.is_none());

        let rejected = state.apply(&Request::SetVolumeSyncRefusal {
            id: VolumeId::new("fast", "nope"),
            refusal: None,
        });
        assert!(rejected.error.unwrap().contains("unknown volume"));
    }

    #[test]
    fn freezing_a_volume_blocks_a_second_freeze_and_unfreeze_makes_the_node_primary() {
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

        assert!(state
            .apply(&Request::FreezeVolume {
                id: id.clone(),
                node: "n2".into(),
                stopped: vec![],
            })
            .is_ok());
        assert!(state
            .apply(&Request::FreezeVolume {
                id: id.clone(),
                node: "n1".into(),
                stopped: vec![],
            })
            .error
            .unwrap()
            .contains("already frozen"));

        assert!(state
            .apply(&Request::UnfreezeVolume { id: id.clone() })
            .is_ok());
        let volume = &state.volumes[&id];
        assert!(volume.frozen.is_none());
        assert_eq!(volume.primary, "n2");
        assert_eq!(volume.generation, 2);
        assert_eq!(state.replicas_behind(&id), ["n1"]);
        assert!(state
            .apply(&Request::UnfreezeVolume { id })
            .error
            .unwrap()
            .contains("not frozen"));
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
    fn an_unused_volume_can_be_removed_and_every_holder_must_ack() {
        let mut state = cluster();
        let id = VolumeId::new("fast", "data");
        state.apply(&Request::UpsertVolume {
            id: id.clone(),
            primary: "n1".into(),
        });
        assert!(state
            .apply(&Request::RemoveVolume { id: id.clone() })
            .is_ok());
        assert!(!state.volumes.contains_key(&id));
        let pending = state.volume_removals[&id].clone();
        assert!(pending.contains("n1"));

        for node in &pending {
            state.apply(&Request::AckVolumeRemoved {
                id: id.clone(),
                node: node.clone(),
            });
        }
        assert!(state.volume_removals.is_empty());
    }

    #[test]
    fn a_mounted_volume_cannot_be_removed() {
        let mut state = cluster();
        let id = VolumeId::new("fast", "data");
        state.apply(&Request::UpsertVolume {
            id: id.clone(),
            primary: "n1".into(),
        });
        state.apply(&Request::SetVolumeEmpty {
            id: id.clone(),
            empty: true,
        });
        state.apply(&Request::DeployStack {
            name: "site".into(),
            spec: "services:\n  web:\n    image: nginx\n    volumes:\n      - fast/data:/data\n"
                .into(),
            at: 0,
        });
        assign(&mut state, "n1", &["fast"]);
        state.apply(&Request::Assign {
            instance: InstanceId::new("site", "web", 1),
            node: "n1".into(),
            drives: drives(&["fast"]),
        });
        let refused = state.apply(&Request::RemoveVolume { id: id.clone() });
        assert!(
            refused.error.is_some_and(|e| e.contains("still mounted")),
            "a mounted volume must not be deleted"
        );
        assert!(state.volumes.contains_key(&id));
    }

    #[test]
    fn a_volume_declared_by_a_stack_cannot_be_removed_even_unassigned() {
        let mut state = cluster();
        let id = VolumeId::new("fast", "data");
        state.apply(&Request::UpsertVolume {
            id: id.clone(),
            primary: "n1".into(),
        });
        state.apply(&Request::DeployStack {
            name: "site".into(),
            spec: "services:\n  web:\n    image: nginx\n    volumes:\n      - fast/data:/data\n"
                .into(),
            at: 0,
        });
        let refused = state.apply(&Request::RemoveVolume { id: id.clone() });
        assert!(refused
            .error
            .is_some_and(|e| e.contains("used by stack site")));
        assert!(state.volumes.contains_key(&id));
    }

    #[test]
    fn volumes_only_live_on_implementing_nodes() {
        let mut state = cluster();
        let rejected = state.apply(&Request::UpsertVolume {
            id: VolumeId::new("bulk", "data"),
            primary: "n2".into(),
        });
        assert!(rejected
            .error
            .unwrap()
            .contains("does not implement drive bulk"));
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

    /// The self-update deploy key rides the ordinary secrets map under a reserved name; `redacted()` needs no special case.
    #[test]
    fn redacted_blanks_the_self_update_deploy_key_like_any_other_secret() {
        let mut state = ClusterState::default();
        state.apply(&Request::PutSecret {
            name: "_clouduler_self_update_deploy_key".into(),
            sealed: base64_encode(b"ssh-private-key-ciphertext"),
            key_fingerprint: "abc123".into(),
            owner: None,
        });
        assert!(
            state.redacted().secrets["_clouduler_self_update_deploy_key"]
                .sealed
                .is_empty()
        );
    }

    #[test]
    fn redacted_drops_login_sessions_and_the_addresses_saved_in_them() {
        let mut state = ClusterState::default();
        state.apply(&Request::PutProxySession {
            key: "k".into(),
            session: ProxySessionRecord {
                host: format!("{REMEMBERED_IP_HOST_PREFIX}public:203.0.113.7"),
                subject: "s".into(),
                username: "alice".into(),
                groups: vec![],
                expires_at: i64::MAX / 4,
                parent: String::new(),
            },
            now: 0,
        });
        assert_eq!(state.proxy_sessions.len(), 1);
        assert!(state.redacted().proxy_sessions.is_empty());
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
            at: 0,
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
            at: 0,
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
            at: 0,
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
            at: 0,
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
        assert_eq!(
            state.cert_status.get("app.example.com"),
            Some(&ReconcileStatus::Pending)
        );

        state.apply(&Request::StoreCertificate(CertificateState {
            fqdn: "app.example.com".into(),
            chain_pem: "chain".into(),
            key_pem: "key".into(),
            not_after: 0,
            issued_at: 0,
        }));

        assert!(state.certificates.contains_key("app.example.com"));
        assert!(!state.cert_status.contains_key("app.example.com"));
    }

    #[test]
    fn clearing_reconcile_status_drops_cert_and_dns_entries() {
        let mut state = ClusterState::default();
        let err = ReconcileStatus::Error {
            message: "no zone".into(),
            at: 0,
        };
        state.apply(&Request::SetCertStatus {
            fqdn: "typo.example.com".into(),
            status: err.clone(),
        });
        state.apply(&Request::SetDnsStatus {
            fqdn: "typo.example.com".into(),
            status: err,
        });
        state.apply(&Request::ClearReconcileStatus {
            fqdn: "typo.example.com".into(),
        });
        assert!(state.cert_status.is_empty());
        assert!(state.dns_status.is_empty());
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

        assert!(state
            .dns_records
            .values()
            .any(|r| r.fqdn == "app.example.com"));
        assert!(!state.dns_status.contains_key("app.example.com"));
    }

    /// A cluster with "fast" declared in the replicated config so `drive_known` sees it.
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

    fn backup_target(
        id: &str,
        drive: &str,
        destination_id: &str,
        enabled: bool,
    ) -> DriveBackupTarget {
        DriveBackupTarget {
            id: id.to_string(),
            drive: drive.to_string(),
            destination_id: destination_id.to_string(),
            enabled,
            frequency_secs: 86_400,
            time_of_day: None,
            keep_last: Some(7),
            keep_within_secs: None,
            keep_daily: None,
            keep_weekly: None,
            keep_monthly: None,
            keep_yearly: None,
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
        state.apply(&Request::PutBackupDestination(node_local_destination(
            "d1", "n1",
        )));

        let unknown_drive = state.apply(&Request::PutBackupTarget(backup_target(
            "t1", "nowhere", "d1", true,
        )));
        assert!(unknown_drive
            .error
            .unwrap()
            .contains("unknown drive nowhere"));

        let unknown_destination = state.apply(&Request::PutBackupTarget(backup_target(
            "t1", "fast", "nowhere", true,
        )));
        assert!(unknown_destination
            .error
            .unwrap()
            .contains("unknown backup destination nowhere"));

        assert!(state
            .apply(&Request::PutBackupTarget(backup_target(
                "t1", "fast", "d1", true
            )))
            .is_ok());
        assert!(state.restic_targets.contains_key("t1"));
    }

    #[test]
    fn a_zero_frequency_target_is_rejected() {
        let mut state = cluster_with_known_drive();
        state.apply(&Request::PutBackupDestination(node_local_destination(
            "d1", "n1",
        )));
        let mut target = backup_target("t1", "fast", "d1", true);
        target.frequency_secs = 0;
        let rejected = state.apply(&Request::PutBackupTarget(target));
        assert!(rejected.error.unwrap().contains("frequency_secs"));
        assert!(!state.restic_targets.contains_key("t1"));
    }

    #[test]
    fn an_ssh_destination_with_an_option_injecting_host_is_rejected() {
        let mut state = cluster();
        let rejected = state.apply(&Request::PutBackupDestination(BackupDestination {
            id: "d1".into(),
            name: "d1".into(),
            kind: BackupDestinationKind::Ssh {
                host: "-oProxyCommand=touch".into(),
                port: 22,
                user: "u".into(),
                path: "/p".into(),
            },
            password_secret: "_clouduler_restic_password_d1".into(),
            credentials_secret: None,
        }));
        assert!(rejected.error.unwrap().contains("ssh host"));
        assert!(state.restic_destinations.is_empty());
    }

    #[test]
    fn two_enabled_targets_cannot_share_a_drive_and_destination() {
        let mut state = cluster_with_known_drive();
        state.apply(&Request::PutBackupDestination(node_local_destination(
            "d1", "n1",
        )));
        state.apply(&Request::PutBackupTarget(backup_target(
            "t1", "fast", "d1", true,
        )));

        let rejected = state.apply(&Request::PutBackupTarget(backup_target(
            "t2", "fast", "d1", true,
        )));
        assert!(rejected
            .error
            .unwrap()
            .contains("already has an enabled backup target"));

        // A disabled duplicate is fine (e.g. a draft being edited).
        assert!(state
            .apply(&Request::PutBackupTarget(backup_target(
                "t2", "fast", "d1", false
            )))
            .is_ok());
    }

    #[test]
    fn removing_a_destination_in_use_by_a_target_is_refused() {
        let mut state = cluster_with_known_drive();
        state.apply(&Request::PutBackupDestination(node_local_destination(
            "d1", "n1",
        )));
        state.apply(&Request::PutBackupTarget(backup_target(
            "t1", "fast", "d1", true,
        )));

        let rejected = state.apply(&Request::RemoveBackupDestination { id: "d1".into() });
        assert!(rejected
            .error
            .unwrap()
            .contains("still used by backup target t1"));

        state.apply(&Request::RemoveBackupTarget { id: "t1".into() });
        assert!(state
            .apply(&Request::RemoveBackupDestination { id: "d1".into() })
            .is_ok());
        assert!(!state.restic_destinations.contains_key("d1"));
    }

    #[test]
    fn setting_last_run_updates_only_the_named_target() {
        let mut state = cluster_with_known_drive();
        state.apply(&Request::PutBackupDestination(node_local_destination(
            "d1", "n1",
        )));
        state.apply(&Request::PutBackupTarget(backup_target(
            "t1", "fast", "d1", true,
        )));

        assert!(state
            .apply(&Request::SetBackupTargetLastRun {
                id: "t1".into(),
                at: 100
            })
            .is_ok());
        assert_eq!(state.restic_targets["t1"].last_run, Some(100));

        let rejected = state.apply(&Request::SetBackupTargetLastRun {
            id: "nowhere".into(),
            at: 0,
        });
        assert!(rejected
            .error
            .unwrap()
            .contains("unknown backup target nowhere"));
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
        assert!(state
            .apply(&Request::RecordBackupRun(first.clone()))
            .is_ok());

        let second = BackupRun {
            id: "run2".into(),
            ..first.clone()
        };
        let rejected = state.apply(&Request::RecordBackupRun(second));
        assert!(rejected.error.unwrap().contains("already running"));

        // A progress update against the same run id is not a new run and is
        // always allowed.
        let mut progressed = first.clone();
        progressed.outcome = BackupRunOutcome::Running { percent: Some(0.5) };
        assert!(state.apply(&Request::RecordBackupRun(progressed)).is_ok());

        // Finishing the first run frees the target up for a new one.
        let mut finished = first;
        finished.outcome = BackupRunOutcome::Succeeded {
            snapshot_id: "abc".into(),
            bytes: 0,
            warning: None,
        };
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
                    warning: None,
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
        assert!(state
            .restic_runs
            .contains_key(&format!("run{}", MAX_BACKUP_RUNS_PER_TARGET + 4)));
    }

    fn restore_request(id: &str, drive: &str) -> RestoreRequest {
        RestoreRequest {
            id: id.to_string(),
            scope: RestoreScope::Drive {
                drive: drive.to_string(),
            },
            destination_id: None,
            snapshot: None,
            requested_at: 0,
            finished_at: None,
            status: RestoreStatus::Pending,
            requested_by: "tester".into(),
        }
    }

    #[test]
    fn clear_errors_drops_failures_and_keeps_everything_else() {
        let mut state = cluster_with_known_drive();
        for id in ["failed", "pending"] {
            assert!(state
                .apply(&Request::PutRestoreRequest(restore_request(id, "fast")))
                .is_ok());
        }
        state.restore_requests.get_mut("failed").unwrap().status =
            RestoreStatus::Failed { error: "x".into() };
        state.cert_status.insert(
            "a.example.com".into(),
            ReconcileStatus::Error {
                message: "x".into(),
                at: 1,
            },
        );
        state
            .dns_status
            .insert("b.example.com".into(), ReconcileStatus::Pending);

        assert!(state.apply(&Request::ClearErrors).is_ok());

        assert!(!state.restore_requests.contains_key("failed"));
        assert!(state.restore_requests.contains_key("pending"));
        assert!(state.cert_status.is_empty());
        assert!(state.dns_status.contains_key("b.example.com"));
    }

    #[test]
    fn a_restore_request_must_name_a_known_drive() {
        let mut state = cluster();
        let rejected = state.apply(&Request::PutRestoreRequest(restore_request(
            "r1", "nowhere",
        )));
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
            .apply(&Request::SetRestoreStatus {
                id: "r1".into(),
                status: RestoreStatus::Running,
                at: 10
            })
            .is_ok());
        let rejected = state.apply(&Request::SetRestoreStatus {
            id: "r1".into(),
            status: RestoreStatus::Running,
            at: 11,
        });
        assert!(rejected.error.unwrap().contains("not pending"));
    }

    #[test]
    fn a_terminal_restore_status_cannot_be_advanced_further() {
        let mut state = cluster_with_known_drive();
        state.apply(&Request::PutRestoreRequest(restore_request("r1", "fast")));
        state.apply(&Request::SetRestoreStatus {
            id: "r1".into(),
            status: RestoreStatus::Running,
            at: 10,
        });
        assert!(state
            .apply(&Request::SetRestoreStatus {
                id: "r1".into(),
                status: RestoreStatus::Failed {
                    error: "boom".into()
                },
                at: 20,
            })
            .is_ok());
        assert_eq!(state.restore_requests["r1"].finished_at, Some(20));

        let rejected = state.apply(&Request::SetRestoreStatus {
            id: "r1".into(),
            status: RestoreStatus::Running,
            at: 30,
        });
        assert!(rejected.error.unwrap().contains("already finished"));
    }

    fn set_update(state: &mut ClusterState, phase: UpdatePhase, at: i64) -> Response {
        state.apply(&Request::SetInstanceUpdate {
            instance: InstanceId::new("gitlab", "web", 0),
            node: "n1".into(),
            phase,
            error: None,
            at,
        })
    }

    #[test]
    fn an_update_keeps_its_start_time_across_phases_and_restarts_it_once_finished() {
        let mut state = cluster();
        assert!(assign(&mut state, "n1", &[]).is_ok());
        let instance = InstanceId::new("gitlab", "web", 0);

        assert!(set_update(&mut state, UpdatePhase::Queued, 10).is_ok());
        assert!(set_update(&mut state, UpdatePhase::Pulling, 20).is_ok());
        assert!(set_update(&mut state, UpdatePhase::Started, 30).is_ok());
        let entry = &state.instance_updates[&instance];
        assert_eq!(
            (entry.phase, entry.started_at, entry.at),
            (UpdatePhase::Started, 10, 30)
        );

        assert!(set_update(&mut state, UpdatePhase::Queued, 40).is_ok());
        assert_eq!(state.instance_updates[&instance].started_at, 40);
    }

    #[test]
    fn auto_update_record_keeps_detection_time_per_digest() {
        let mut state = cluster();
        assert!(assign(&mut state, "n1", &[]).is_ok());
        let instance = InstanceId::new("gitlab", "web", 0);
        let set = |state: &mut ClusterState, digest: &str, outcome, at| {
            state.apply(&Request::SetAutoUpdate {
                instance: InstanceId::new("gitlab", "web", 0),
                digest: digest.into(),
                outcome,
                error: None,
                at,
            })
        };
        assert!(set(&mut state, "sha256:a", AutoUpdateOutcome::Waiting, 10).is_ok());
        assert!(set(&mut state, "sha256:a", AutoUpdateOutcome::Queued, 50).is_ok());
        let rec = &state.auto_updates[&instance];
        assert_eq!((rec.detected_at, rec.at), (10, 50));
        // A newer digest restarts the countdown.
        assert!(set(&mut state, "sha256:b", AutoUpdateOutcome::Waiting, 60).is_ok());
        assert_eq!(state.auto_updates[&instance].detected_at, 60);
        // Survives clearing errors, goes with the instance.
        state.apply(&Request::ClearErrors);
        assert!(state.auto_updates.contains_key(&instance));
        state.apply(&Request::Unassign {
            instance: instance.clone(),
        });
        assert!(state.auto_updates.is_empty());
        assert!(!set(&mut state, "sha256:c", AutoUpdateOutcome::Waiting, 70).is_ok());
    }

    #[test]
    fn image_checks_replace_wholesale_but_keep_a_digest_through_an_error() {
        let mut state = cluster();
        let check = |digest: Option<&str>, error: Option<&str>| ImageCheck {
            latest_digest: digest.map(String::from),
            checked_at: 1,
            error: error.map(String::from),
        };
        state.apply(&Request::SetImageChecks {
            checks: BTreeMap::from([
                ("a:1".to_string(), check(Some("sha256:a"), None)),
                ("b:1".to_string(), check(Some("sha256:b"), None)),
            ]),
            at: 10,
        });
        state.apply(&Request::SetImageChecks {
            checks: BTreeMap::from([("a:1".to_string(), check(None, Some("429")))]),
            at: 20,
        });
        assert_eq!(state.image_checks_at, 20);
        assert_eq!(state.image_checks.len(), 1);
        let a = &state.image_checks["a:1"];
        assert_eq!(a.latest_digest.as_deref(), Some("sha256:a"));
        assert_eq!(a.error.as_deref(), Some("429"));
    }

    #[test]
    fn crash_moves_accumulate_prune_and_clear() {
        let mut state = cluster();
        assert!(assign(&mut state, "n1", &[]).is_ok());
        let instance = InstanceId::new("gitlab", "web", 0);
        let record = |state: &mut ClusterState, from: &str, at: u64| {
            state.apply(&Request::RecordCrashMove {
                instance: instance.clone(),
                from: from.into(),
                at,
            })
        };

        assert!(record(&mut state, "n1", 100).is_ok());
        assert!(record(&mut state, "n2", 200).is_ok());
        assert_eq!(state.crash_moves[&instance].len(), 2);

        // A record far enough later drops the stale entries.
        assert!(record(&mut state, "n3", 200 + CRASH_MOVE_RETENTION_SECS + 1).is_ok());
        assert_eq!(
            state.crash_moves[&instance].keys().collect::<Vec<_>>(),
            vec!["n3"]
        );

        // An instance with no assignment cannot leave an entry behind.
        let rejected = state.apply(&Request::RecordCrashMove {
            instance: InstanceId::new("gitlab", "ghost", 0),
            from: "n1".into(),
            at: 1,
        });
        assert!(rejected.error.is_some());

        // A changed spec forgives every node; an identical redeploy does not.
        state.apply(&Request::DeployStack {
            name: "gitlab".into(),
            spec: "a".into(),
            at: 0,
        });
        assert!(record(&mut state, "n1", 5).is_ok());
        state.apply(&Request::DeployStack {
            name: "gitlab".into(),
            spec: "a".into(),
            at: 0,
        });
        assert!(state.crash_moves.contains_key(&instance));
        state.apply(&Request::DeployStack {
            name: "gitlab".into(),
            spec: "b".into(),
            at: 0,
        });
        assert!(state.crash_moves.is_empty());

        assert!(record(&mut state, "n1", 5).is_ok());
        state.apply(&Request::Unassign {
            instance: instance.clone(),
        });
        assert!(state.crash_moves.is_empty());
    }

    #[test]
    fn update_entries_go_away_with_their_instance() {
        let mut state = cluster();
        assert!(!set_update(&mut state, UpdatePhase::Pulling, 1).is_ok());
        assert!(state.instance_updates.is_empty());

        assert!(assign(&mut state, "n1", &[]).is_ok());
        assert!(set_update(&mut state, UpdatePhase::Pulling, 1).is_ok());
        state.apply(&Request::Unassign {
            instance: InstanceId::new("gitlab", "web", 0),
        });
        assert!(state.instance_updates.is_empty());
    }

    fn running_run(id: &str, started_at: i64) -> BackupRun {
        BackupRun {
            id: id.into(),
            target_id: format!("t-{id}"),
            drive: "fast".into(),
            destination_id: "d1".into(),
            started_at,
            finished_at: None,
            outcome: BackupRunOutcome::Running { percent: None },
            triggered_by: BackupTrigger::Schedule,
        }
    }

    #[test]
    fn run_pruning_never_drops_the_new_entry_or_a_running_one() {
        let mut state = cluster_with_known_drive();
        // An old Running run, then a full set of finished runs newer than it.
        assert!(state
            .apply(&Request::RecordBackupRun(running_run("old-running", 0)))
            .is_ok());
        for i in 0..MAX_BACKUP_RUNS_PER_TARGET {
            let mut r = running_run(&format!("done{i}"), 100 + i as i64);
            r.outcome = BackupRunOutcome::Succeeded {
                snapshot_id: "s".into(),
                bytes: 0,
                warning: None,
            };
            assert!(state.apply(&Request::RecordBackupRun(r)).is_ok());
        }
        // A new entry with the oldest timestamp of all.
        let mut ancient = running_run("ancient", -5);
        ancient.outcome = BackupRunOutcome::Succeeded {
            snapshot_id: "s".into(),
            bytes: 0,
            warning: None,
        };
        assert!(state.apply(&Request::RecordBackupRun(ancient)).is_ok());
        assert!(state.restic_runs.contains_key("ancient"));
        assert!(state.restic_runs.contains_key("old-running"));
        assert_eq!(state.restic_runs.len(), MAX_BACKUP_RUNS_PER_TARGET);
        assert!(!state.restic_runs.contains_key("done0"));
    }

    #[test]
    fn restore_pruning_never_drops_the_new_entry() {
        let mut state = cluster_with_known_drive();
        for i in 0..MAX_RESTORE_REQUESTS {
            let mut r = restore_request(&format!("r{i}"), "fast");
            r.requested_at = 100 + i as i64;
            assert!(state.apply(&Request::PutRestoreRequest(r)).is_ok());
        }
        let mut ancient = restore_request("ancient", "fast");
        ancient.requested_at = -1;
        assert!(state.apply(&Request::PutRestoreRequest(ancient)).is_ok());
        assert!(state.restore_requests.contains_key("ancient"));
        assert_eq!(state.restore_requests.len(), MAX_RESTORE_REQUESTS);
    }

    #[test]
    fn remove_node_reassigns_volume_primary_and_purges_container_status() {
        let mut state = cluster();
        let id = VolumeId::new("fast", "data");
        assert!(state
            .apply(&Request::UpsertVolume {
                id: id.clone(),
                primary: "n1".into(),
            })
            .is_ok());
        assert!(state
            .apply(&Request::SetVolumeReplica {
                id: id.clone(),
                node: "n2".into(),
                generation: 1,
            })
            .is_ok());
        state.apply(&Request::ReportContainerStatus {
            node: "n1".into(),
            running: BTreeMap::from([(InstanceId::new("gitlab", "web", 0), true)]),
            secret_revisions: BTreeMap::new(),
            config_revisions: BTreeMap::new(),
        });
        assert!(!state.container_status.is_empty());

        assert!(state
            .apply(&Request::RemoveNode { node: "n1".into() })
            .is_ok());
        assert_eq!(state.volumes[&id].primary, "n2");
        assert!(!state.volumes[&id].replicas.contains_key("n1"));
        assert!(state.container_status.is_empty());
    }

    #[test]
    fn remove_node_with_no_other_replica_still_succeeds() {
        let mut state = cluster();
        let id = VolumeId::new("fast", "data");
        state.apply(&Request::UpsertVolume {
            id: id.clone(),
            primary: "n1".into(),
        });
        assert!(state
            .apply(&Request::RemoveNode { node: "n1".into() })
            .is_ok());
        assert!(state.volumes[&id].replicas.is_empty());
    }

    #[test]
    fn reassigning_keeps_an_operator_stop_and_drops_a_stale_migration() {
        let mut state = cluster();
        let instance = InstanceId::new("gitlab", "web", 0);
        assert!(assign(&mut state, "n1", &[]).is_ok());
        state.apply(&Request::SetDesiredState {
            instance: instance.clone(),
            desired: DesiredState::Stopped,
        });
        assert!(assign(&mut state, "n2", &[]).is_ok());
        assert_eq!(state.assignments[&instance].node, "n2");
        assert_eq!(state.assignments[&instance].desired, DesiredState::Stopped);

        // A migration in flight survives a same-node re-Assign but not a move.
        assert!(state
            .apply(&Request::BeginMigration {
                instance: instance.clone(),
                to: "n1".into(),
                drives: Default::default(),
            })
            .is_ok());
        assert!(assign(&mut state, "n2", &[]).is_ok());
        assert!(state.assignments[&instance].migration.is_some());
        assert!(assign(&mut state, "n3", &[]).is_ok());
        assert!(state.assignments[&instance].migration.is_none());
    }

    #[test]
    fn a_container_report_carries_secret_revisions_and_a_bare_one_clears_them() {
        let mut state = ClusterState::default();
        let id = InstanceId::new("app", "web", 0);
        state.apply(&Request::ReportContainerStatus {
            node: "n1".into(),
            running: BTreeMap::from([(id.clone(), true)]),
            secret_revisions: BTreeMap::from([(
                id.clone(),
                BTreeMap::from([("db".to_string(), 2)]),
            )]),
            config_revisions: BTreeMap::new(),
        });
        assert_eq!(state.container_status[&id].secret_revisions["db"], 2);

        // A report from a node that predates the field omits it.
        let request: Request = serde_json::from_str(
            r#"{"ReportContainerStatus":{"node":"n1","running":{"app.web.0":true}}}"#,
        )
        .expect("old-shape report");
        state.apply(&request);
        assert!(state.container_status[&id].secret_revisions.is_empty());
    }
}
