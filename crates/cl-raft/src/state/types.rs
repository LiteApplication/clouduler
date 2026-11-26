use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum Availability {
    Active,
    /// Keeps running what it has, takes nothing new. Set before a migration.
    Drain,
    Decommissioned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Manager,
    Worker,
}

/// A node as the cluster sees it; seeded from config, edited at runtime via the ui.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
    /// Blake3 hex of the ui bundle this node serves (the name of its `self-update/ui/<hash>`
    /// directory); `None` on a worker or a node with no ui staged for its running version.
    #[serde(default)]
    pub ui_hash: Option<String>,
    /// Unix seconds this node's daemon process started, self-reported; 0 until reported. A forced
    /// self-update reinstall confirms a restart by this moving forward, since the binary hash
    /// does not change.
    #[serde(default)]
    pub started_at: i64,
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct NvidiaGpu {
    pub index: u32,
    pub uuid: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RenderNode {
    pub path: String,
    #[serde(default)]
    pub driver: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum PortProtocol {
    Tcp,
    Udp,
}

/// A deployed stack; the spec is the raw submitted yaml so the ui can round trip it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ClusterConfigEntry {
    pub yaml: String,
    /// Bumped on every accepted edit; a node writes the file only when this moves.
    pub generation: u64,
}

/// A cluster wide config object, mounted read only into referencing containers. Stored in the clear.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
    cl_config::check_object_name(name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum DesiredState {
    Running,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Assignment {
    pub instance: InstanceId,
    pub node: String,
    pub desired: DesiredState,
    /// Pinned image digest, set once pulled so all replicas run the same bits even for `:latest`.
    pub image_digest: Option<String>,
    pub migration: Option<Migration>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Migration {
    /// The node driving the move (where the instance started). Recorded because
    /// `Switching` repoints the assignment at the target, after which a node that
    /// crashed mid-move could not otherwise recognise its own job.
    pub from: String,
    pub to: String,
    pub phase: MigrationPhase,
    /// An operator asked to skip the `Verifying` content check (`SkipMigrationVerify`).
    /// Only ever set while verifying, so the driver advances to `Switching` without
    /// comparing the target copy.
    #[serde(default)]
    pub skip_verify: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SyncRefusal {
    pub from: String,
    pub to: String,
    pub reason: String,
    /// Unix seconds, stamped by the refusing node to keep `apply` clock free.
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum ReconcileStatus {
    Pending,
    Error { message: String, at: i64 },
}

/// A place restic can keep a drive's backup history; see `DriveBackupTarget`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum BackupTrigger {
    Schedule,
    Manual { by: String },
}

/// Newest runs kept per `(drive, destination_id)`; bounded like `MAX_OBJECT_BYTES` since history goes through the raft log and snapshots.
pub const MAX_BACKUP_RUNS_PER_TARGET: usize = 50;

/// What a restore covers: a drive, one volume, or every volume a service uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum RestoreScope {
    Drive { drive: String },
    Volume { volume: VolumeId },
    Service { stack: String, service: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ImageCheck {
    /// The registry's digest for the reference; kept from the previous check when the latest lookup failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_digest: Option<String>,
    pub checked_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
