// Types mirror the JSON the Rust side emits. Field names match the Rust
// struct fields (serde uses them as-is, no camelCase conversion), and enum
// values match `#[serde(rename_all = "lowercase")]`. Keep this file the one
// place that knows the wire shape; nothing else should guess at it.

export type Role = 'manager' | 'worker';
export type Availability = 'active' | 'drain' | 'decommissioned';
export type PortProtocol = 'tcp' | 'udp';

export interface NodeState {
  name: string;
  role: Role;
  rpc_addr: string;
  /** drive name -> local backing path */
  drives: Record<string, string>;
  availability: Availability;
  /** Labels from the cluster config, matched by
   *  `deploy.placement.constraints` as `node.labels.<key>`. */
  labels: Record<string, string>;
  /** Egress-kind networks this node has a host interface address for. */
  egress_networks: string[];
  /** Every network this node has any address on. */
  networks: string[];
  /** network name -> ports this node has open on it (public router
   *  forwarding, or a wireguard tunnel's far end forwarding a port back). */
  open_ports: Record<string, [PortProtocol, number][]>;
  /** std::env::consts::ARCH of the running binary, e.g. "x86_64". Empty until
   *  the node has reported in at least once since this field was added. */
  arch: string;
  /** Blake3 hash (hex) of the currently running binary. */
  binary_hash: string;
  /** This node's X25519 public key, base64. Null until it has reported in at
   *  least once. Lets a key-holding node deliver the cluster key to it
   *  without the plaintext ever crossing the network. */
  node_pubkey: string | null;
  /** Total system memory in bytes, self-reported alongside arch/binary_hash.
   *  0 until the node has reported in at least once. */
  memory_total_bytes: number;
  /** Version of the running binary, e.g. "0.14.2". Absent until the node has
   *  reported in since this field was added. */
  version?: string;
  /** GPUs and device nodes the node found on itself. Absent on a node too old
   *  to report them, which the scheduler treats as having none. */
  gpus?: NodeGpus;
}

export interface NvidiaGpu {
  index: number;
  uuid: string;
  name: string;
}

export interface RenderNode {
  path: string;
  /** Kernel driver (`i915`, `xe`, `amdgpu`, ...); empty when unknown. */
  driver: string;
}

/** Mirrors `cl_raft::state::NodeGpus`. */
export interface NodeGpus {
  /** `/dev/nvidia[0-9]*` device nodes. */
  nvidia_devices: string[];
  /** GPUs `nvidia-smi` listed; may be empty while `nvidia_devices` is not. */
  nvidia: NvidiaGpu[];
  /** Docker has an `nvidia` runtime (NVIDIA container toolkit configured). */
  nvidia_runtime: boolean;
  /** Path of nvidia-container-runtime-hook or nvidia-container-cli, when one
   *  was found; docker can serve NVIDIA requests with only this. */
  nvidia_hook?: string | null;
  render_nodes: RenderNode[];
  /** Host paths a service's `devices:` entry may name. */
  dev_paths: string[];
}

/** Per-drive space on one node: the drive's configured path matched to the
 *  longest-prefix mount. */
export interface DriveUsage {
  drive: string;
  path: string;
  total_bytes: number;
  free_bytes: number;
}

export interface ResourceUsage {
  cpu_count: number;
  load_1m: number;
  memory_total_bytes: number;
  memory_used_bytes: number;
  uptime_seconds: number;
  /** False (all-zero above) unless the answering node is this node itself. */
  local: boolean;
  /** CPU use 0-100 from the node's 5 s sampler. Absent on an older daemon. */
  cpu_percent?: number;
  /** Binary version of the node. Absent on an older daemon. */
  version?: string;
  drives?: DriveUsage[];
}

/** One row of GET /api/nodes/metrics; unreachable nodes are skipped. */
export interface NodeMetricsRow {
  name: string;
  container_count: number;
  usage: ResourceUsage;
}
export type NodeMetrics = NodeMetricsRow[];

/** One point of GET /api/nodes/:name/metrics/history (5 s apart, last 120). */
export interface NodeMetricSample {
  /** Unix ms. */
  at: number;
  cpu_percent: number;
  memory_used_bytes: number;
}

/** Response of GET /api/nodes/:name. */
export interface NodeDetail {
  name: string;
  role: Role;
  availability: Availability;
  reachable: boolean;
  drives: string[];
  container_count: number;
  usage: ResourceUsage;
}

/** What one node sees of one peer (GET /api/nodes/connectivity). */
export interface PeerLink {
  peer: string;
  /** An authenticated raft ping from the reporting node to the peer worked. */
  raft: boolean;
  /** Same, for the peer's web/api port. */
  api: boolean;
  /** Round trip of the raft connect in ms; null when it failed. */
  raft_ms: number | null;
}

/** One node's own report. `reported` is false when the node could not be
 *  asked, in which case `links` is empty and says nothing about its peers. */
export interface NodeReport {
  node: string;
  reported: boolean;
  links: PeerLink[];
  /** Milliseconds a small durable write (write + fsync) took on this node's
   *  data dir. Raft followers must fsync every append, so seconds here means
   *  the node answers pings but cannot keep up with the leader. Null when
   *  unreported or the probe failed. */
  disk_ms?: number | null;
}

/** Response of GET /api/nodes/connectivity. */
export interface Connectivity {
  reports: NodeReport[];
}

export type DesiredState = 'running' | 'stopped';

export type MigrationPhase =
  | 'stopping'
  | 'syncing'
  | 'verifying'
  | 'switching'
  | 'starting'
  | 'done'
  | { failed: { reason: string } };

export interface Migration {
  to: string;
  phase: MigrationPhase;
}

/** InstanceId serializes as "stack.service.index". */
export type InstanceId = string;
/** VolumeId serializes as "drive/volume". */
export type VolumeIdStr = string;

export interface Assignment {
  instance: InstanceId;
  node: string;
  desired: DesiredState;
  image_digest: string | null;
  migration: Migration | null;
}

/** What GET /api/instances returns: the assignment plus live container state.
 *  `state` is "unknown" for an instance on another node: there is no rpc path
 *  to inspect a container elsewhere, and that is different from "stopped".
 *
 *  `pulling` is a container that does not exist yet because its image is
 *  still being fetched; `pending_placement` is not a container at all, but a
 *  service the cluster wants to run and has not found a node for yet — see
 *  `placement_error`. `stopping`/`restarting`/`exited` come straight from
 *  docker's own lifecycle status (`exited` only fires when the cluster wanted
 *  it running, i.e. it crashed rather than being asked to stop). */
export type MigrationProblemKind =
  | 'verify_mismatch'
  | 'sync_refused'
  | 'sync_failed'
  | 'stop_failed'
  | 'start_failed'
  | 'other';

export interface MigrationCoWriter {
  instance: string;
  node: string;
  running: boolean;
}

export interface MigrationAction {
  id: string;
  label: string;
  description: string;
  primary: boolean;
  danger: boolean;
  confirm?: string;
}

export interface MigrationProblem {
  kind: MigrationProblemKind;
  title: string;
  detail: string;
  volume?: string;
  added?: string[];
  changed?: string[];
  removed?: string[];
  files_total?: number;
  co_writers?: MigrationCoWriter[];
  from: string;
  to: string;
  raw: string;
  actions: MigrationAction[];
}

export interface InstanceState {
  id: InstanceId;
  node: string;
  desired: DesiredState;
  state: 'running' | 'stopped' | 'stopping' | 'restarting' | 'exited' | 'pulling' | 'pending_placement' | 'unknown';
  running: boolean | null;
  health: 'healthy' | 'unhealthy' | 'starting' | 'none';
  image: string | null;
  image_digest: string | null;
  /** Registry digest of the bits the container is actually running, as
   *  opposed to `image_digest`, which is what the cluster has pinned. Absent
   *  for an instance running elsewhere, one with no container, or one whose
   *  image was built locally and so has no registry digest. */
  running_digest?: string | null;
  started_at: string | null;
  migration: { to: string; phase: string } | null;
  /** Set when the last move failed: what went wrong in plain words and the
   *  actions that fit. */
  migration_problem?: MigrationProblem;
  /** Why no node could run this instance, set only when `state` is
   *  `"pending_placement"`. */
  placement_error?: string;
  /** The reverse proxy's own most recent active check of this instance's
   *  service, when it publishes a route. Absent means either nothing is
   *  published for it or no check has landed yet — not that it's healthy;
   *  a present `proxy` with no `error` does mean healthy. */
  proxy?: ProxyHealth;
  /** What is actively happening right now — an update or redeploy step or a
   *  migration phase, composed server-side into one string — or absent when
   *  the instance is steady-state. */
  reason?: string | null;
  /** `[step, total]` alongside `reason`, for a progress bar; absent when
   *  `reason` describes something with no fixed step count (e.g. a
   *  migration phase other than "syncing"). */
  step?: [number, number] | null;
  // The fields below are all absent on an older daemon (and on an instance
  // that lives on another node, for the container ones).
  /** The kernel killed the container for exceeding its memory limit. */
  oom_killed?: boolean;
  /** Exit code of the last run; null while it is running. */
  exit_code?: number | null;
  restart_count?: number;
  /** Restarts allowed before the instance is rescheduled elsewhere. */
  restart_limit?: number | null;
  /** How many times crash-loop handling has moved this instance to another node. */
  crash_moves?: number;
  finished_at?: string | null;
  memory_limit_bytes?: number | null;
}

/** One `progress` event from `GET /api/instances/events`: sent once when an
 *  instance's live status is first seen, then again only when it changes. */
export interface InstanceProgressEvent {
  instance_id: InstanceId;
  state: InstanceState['state'];
  reason: string | null;
  step: [number, number] | null;
}

/** One route's latest active health check: the proxy itself connecting to
 *  the backend, independent of whether any real client traffic has hit it
 *  yet. `error` mirrors the "bad gateway: ..." text a real client would see. */
export interface ProxyHealth {
  host: string;
  checked_at: string;
  error?: string;
}

/** Result of `POST /api/rebalance`: every move it decided on and already
 *  submitted (each went through the same BeginMigration path a manual move
 *  does, so by the time this response arrives they are underway). */
export interface RebalanceResponse {
  moves: { instance: InstanceId; from: string; to: string }[];
}

export interface StackState {
  name: string;
  spec: string;
  revision: number;
  /** Configs/secrets this stack bundles its own file for, i.e. owns: only a
   *  subset of what it uses. Prefer `used_configs`/`used_secrets` for "does
   *  this stack reference this config" checks. */
  configs: string[];
  secrets: string[];
  /** Every config/secret any service in this stack references, whether or
   *  not the stack bundles its own file for it. */
  used_configs: string[];
  used_secrets: string[];
  /** Unix seconds of the last deploy. Absent for a stack deployed before
   *  this was recorded. */
  deployed_at?: number;
  /** The spec this stack ran before its latest deploy, for a "changes since
   *  last deploy" diff. Absent or null on a first deploy. */
  previous_spec?: string | null;
}

/** What a stack needs from the cluster to schedule: computed from its spec by
 *  the server, so the ui never has to parse compose yaml itself. */
export interface StackRequirements {
  networks: string[];
  drives: string[];
  configs: string[];
  secrets: string[];
  /** Top-level configs/secrets this stack bundles its own file for: these are
   *  the ones the editor should offer to upload content for. */
  file_configs: string[];
  file_secrets: string[];
  /** Fully qualified hostnames this stack's services are proxied under.
   *  Cross-reference against `ClusterState.dns_records`/`certificates`/
   *  `dns_status`/`cert_status` to show this stack's DNS/cert state — those
   *  maps carry no stack field of their own. */
  hosts: string[];
  /** `x-clouduler.node`, this service's effective hard pin (its own, or the
   *  stack wide default). Per-service only: `null` on the stack-aggregate
   *  `requirements`, only meaningful on `per_service`. */
  pinned_node: string | null;
  /** Raw `deploy.placement.constraints` entries (Swarm syntax, e.g.
   *  `node.hostname==n1` or `node.labels.fast==true`). Per-service only,
   *  same reasoning as `pinned_node`. */
  placement_constraints: string[];
  /** `x-clouduler.expose` entries. A node needs the port marked open in its
   *  `open_ports` to be a candidate, but only when the network is public or
   *  wireguard kind (a nat/egress/internal network's expose entry, if any,
   *  needs no such check). Per-service only, same reasoning as `pinned_node`. */
  bound_ports: { network: string; protocol: PortProtocol; published: number }[];
  /** Drives mounted whole (`drive/.`) rather than as a named subvolume — a
   *  subset of `drives`. These need the stricter "pinned to the drive's one
   *  real holder" eligibility instead of the ordinary implements-or-served
   *  check every other drive gets. Per-service only, same reasoning as
   *  `pinned_node`. */
  whole_drive_mounts: string[];
  /** `drive/volume` ids of the named volumes mounted. Whole-drive mounts
   *  are in `whole_drive_mounts` instead. */
  volumes?: string[];
  /** Each network the stack declares, with its `allowed` list as written;
   *  `null` means unrestricted. Stack aggregate only (empty on `per_service`). */
  allowed?: Record<string, string[] | null>;
  /** NVIDIA device requests (`deploy.resources.reservations.devices`): the
   *  least GPUs needed and any named ids. A node needs enough GPUs in
   *  `gpus` and the nvidia docker runtime. Per-service only. */
  gpu_requests?: { count: number; device_ids: string[] }[];
  /** Host paths from `devices:`; a node must report each in `gpus.dev_paths`.
   *  Per-service only. */
  host_devices?: string[];
}

/** GET /api/stacks/:name. */
export interface StackDetailResponse extends StackState {
  requirements: StackRequirements;
}

export interface ConfigEntry {
  name: string;
  content: string;
  revision: number;
  owner: string | null;
}

export interface SecretEntry {
  name: string;
  // sealed value is always redacted to "" by the api; never rely on it.
  sealed: string;
  revision: number;
  key_fingerprint: string;
  owner: string | null;
}

export interface CertificateState {
  fqdn: string;
  chain_pem: string;
  key_pem: string;
  not_after: number;
  /** Unix seconds. */
  issued_at?: number;
  /** From the list endpoint, which unions certificates and `cert_status`. */
  status?: 'ready' | 'pending' | 'error';
  error?: string;
  error_at?: number;
}

export interface DnsRecordState {
  fqdn: string;
  record_type: string;
  content: string;
  zone: string;
  provider_id: string | null;
  status?: 'ready' | 'pending' | 'error';
  error?: string;
}

/** Where a certificate or DNS record stands before it has anything to show
 *  for itself in `certificates`/`dns_records` — see the same type in
 *  cl-raft's state.rs. `'Pending'` is issuance/publishing in flight;
 *  `{ Error }` is the last attempt's failure reason and when it happened. */
export type ReconcileStatus = 'Pending' | { Error: { message: string; at: number } };

export interface VolumeState {
  id: VolumeIdStr;
  generation: number;
  primary: string;
  replicas: Record<string, number>;
  /** Set while the volume is being edited by hand and not replicating. */
  frozen: { node: string; stopped: string[] } | null;
  /** Set while the source refuses to replicate because it would wipe a replica. */
  sync_refused: { from: string; to: string; reason: string; at: number } | null;
  /** The source found no files in the volume on two scans in a row. */
  looks_empty: boolean;
}

export interface ClusterState {
  nodes: Record<string, NodeState>;
  stacks: Record<string, StackState>;
  assignments: Record<InstanceId, Assignment>;
  volumes: Record<VolumeIdStr, VolumeState>;
  configs: Record<string, ConfigEntry>;
  secrets: Record<string, SecretEntry>;
  certificates: Record<string, CertificateState>;
  dns_records: Record<string, DnsRecordState>;
  cert_status: Record<string, ReconcileStatus>;
  dns_status: Record<string, ReconcileStatus>;
  unreachable: string[];
  /** Node names removed through the API, kept as a tombstone until the name
   *  is reused, so a stale leader config cannot resurrect them. */
  removed: string[];
  overlay: string | null;
  /** The cluster key, wrapped once per node (see WrappedKeyEntry). Ciphertext
   *  only; the plaintext key never appears in this state. */
  wrapped_keys: Record<string, WrappedKeyEntry>;
  /** The replicated cluster.yaml, null until the first write. Read via
   *  `/api/config` instead; this is raft's own copy. */
  cluster_config: ClusterConfigEntry | null;
  /** Set only by `clouduler emergency`'s offline recovery write: while this
   *  holds a stack name, the scheduler places and starts only that stack,
   *  leaving every other assignment untouched. Cleared automatically once
   *  another manager is reachable again. */
  emergency_stack: string | null;
  restic_destinations: Record<string, BackupDestination>;
  restic_targets: Record<string, DriveBackupTarget>;
  restic_runs: Record<string, BackupRun>;
  restore_requests: Record<string, RestoreRequest>;
  /** Image updates and redeploys in flight or recently finished, written
   *  step by step by the node doing the work. Finished entries are cleared
   *  by that node about 15 minutes later. */
  instance_updates: Record<InstanceId, InstanceUpdate>;
  /** Last docker state each node reported per instance. `secret_revisions`
   *  holds the revision of each cluster secret the container was created
   *  with (absent when it has none or predates the label). */
  container_status?: Record<InstanceId, { node: string; running: boolean; secret_revisions?: Record<string, number> }>;
  /** What auto_update did about each instance's newest digest. */
  auto_updates?: Record<
    InstanceId,
    { digest: string; detected_at: number; outcome: AutoUpdateOutcome; at: number; error?: string }
  >;
  /** The registries' last answer per declared image reference. */
  image_checks?: Record<string, { latest_digest?: string; checked_at: number; error?: string }>;
  /** Unix seconds the last registry check finished, 0 before the first. */
  image_checks_at?: number;
  /** Login sessions keyed by a hash of the cookie; the ui never needs them. */
  sessions?: Record<string, unknown>;
  /** Reverse proxy login sessions and one-time tokens; the ui never needs them. */
  proxy_sessions?: Record<string, unknown>;
  proxy_tokens?: Record<string, unknown>;
  /** Served-drive authority (holder, handover, conflict) for drives that have
   *  failed over; the drive list carries the same facts, the ui does not read this. */
  served?: Record<string, ServedAuthority>;
  version: number;
}

/** The steps of an update, in order. The old container keeps serving
 *  through `queued` and `pulling`; `started` and `failed` are terminal. */
export type UpdatePhase = 'queued' | 'pulling' | 'stopping' | 'starting' | 'started' | 'failed';

export interface InstanceUpdate {
  /** The node whose docker is doing the work. */
  node: string;
  phase: UpdatePhase;
  /** Unix seconds the update began. */
  started_at: number;
  /** Unix seconds of the latest phase change. */
  at: number;
  /** Only on `failed`. */
  error?: string;
}

// ---- restic backups ----

export type BackupDestinationKind =
  | { kind: 'nodelocal'; node: string; path: string }
  | { kind: 'ssh'; host: string; port: number; user: string; path: string }
  | { kind: 's3'; bucket: string; endpoint: string; region?: string | null; prefix?: string | null };

/** A place restic can keep a drive's backup history. A drive may back up to
 *  several of these at once - see DriveBackupTarget. */
export interface BackupDestination {
  id: string;
  name: string;
  kind: BackupDestinationKind;
  password_secret: string;
  credentials_secret?: string | null;
}

/** What GET /api/restic/destinations returns: never the secret names
 *  themselves, just whether credentials beyond the password are set. */
export interface BackupDestinationSummary {
  id: string;
  name: string;
  kind: BackupDestinationKind;
  has_credentials: boolean;
}

/** One drive's backup relationship with one destination. A drive may have
 *  several of these, one per destination it backs up to. */
export interface DriveBackupTarget {
  id: string;
  drive: string;
  destination_id: string;
  enabled: boolean;
  frequency_secs: number;
  /** "HH:MM", 24h, UTC. Null means any time of day once frequency_secs has
   *  elapsed. */
  time_of_day?: string | null;
  keep_last?: number | null;
  keep_within_secs?: number | null;
  /** Overrides the resolved x-clouduler stop-before-backup default when set. */
  stop_containers?: boolean | null;
  /** Unix seconds this target last started a run. Null before its first run. */
  last_run?: number | null;
}

/** `percent` is restic's own last-seen `percent_done` (0-1), updated in
 *  place while the run is in flight; null until the first status line lands. */
export type BackupRunOutcome =
  | { Running: { percent: number | null } }
  | { Succeeded: { snapshot_id: string; bytes: number; warning?: string } }
  | { Failed: { error: string } };

export type BackupTrigger = 'Schedule' | { Manual: { by: string } };

/** One completed, failed, or in-flight backup run, kept as history. */
export interface BackupRun {
  id: string;
  target_id: string;
  drive: string;
  destination_id: string;
  started_at: number;
  finished_at?: number | null;
  outcome: BackupRunOutcome;
  triggered_by: BackupTrigger;
}

export type RestoreScope =
  | { Drive: { drive: string } }
  | { Volume: { volume: VolumeIdStr } }
  | { Service: { stack: string; service: string } };

export type RestoreStatus = 'Pending' | 'Running' | 'Succeeded' | { Failed: { error: string } };

/** A restore submitted through the api. Whichever node actually implements
 *  the target drive claims (Pending -> Running) and executes it. */
export interface RestoreRequest {
  id: string;
  scope: RestoreScope;
  destination_id?: string | null;
  snapshot?: string | null;
  requested_at: number;
  finished_at?: number | null;
  status: RestoreStatus;
  requested_by: string;
}

/** One restic snapshot, as GET /api/restic/destinations/:id/snapshots returns. */
export interface SnapshotInfo {
  id: string;
  time: string;
  tags: string[];
}

/** The replicated cluster.yaml and the generation it was written at. */
export interface ClusterConfigEntry {
  yaml: string;
  generation: number;
}

/** The cluster secret key, encrypted to one node's public key. */
export interface WrappedKeyEntry {
  ephemeral_public: string;
  sealed: string;
  key_fingerprint: string;
  target_pubkey: string;
}

// ---- /api/status ----

export interface ClusterStatus {
  /** The node that answered, which is not necessarily the leader. */
  node: string;
  is_leader: boolean;
  /** Named to match the Rust field. Reading `quorum` here silently yielded
   *  `undefined`, so `!status.quorum` was always true and the overview showed
   *  "Quorum lost" on a perfectly healthy cluster. */
  has_quorum: boolean;
  leader: string | null;
  /** Raft voters, and how many of them this node can currently reach. Not the
   *  same as managers_*: a manager is only a voter once raft has added it. */
  voters: number;
  reachable: number;
  managers_total: number;
  managers_reachable: number;
  version: string;
  state_version: number;
}

// ---- auth ----

export type UserRole = 'admin' | 'viewer';

export interface Identity {
  subject: string;
  username: string;
  role: UserRole;
  /** From /auth/me only: readable by every user, unlike /api/config, so the
   *  nav can hide the AI access page when the operator switched MCP off. */
  mcp_enabled?: boolean;
}

export type McpGrant = 'read' | 'admin';

/** An MCP client (Claude Code, Cursor, ...) an admin or viewer has approved. */
export interface McpClient {
  client_id: string;
  client_name: string;
  grant: McpGrant;
  approved_by: string;
  /** Unix seconds. */
  created_at: number;
}

/** GET /api/oauth/requests/{id}: what the consent page asks the user to approve. */
export interface OAuthRequestInfo {
  client_name: string;
  redirect_host: string;
  scope: string;
  requested_grant: McpGrant;
  allowed_grants: McpGrant[];
}

export interface ApiKey {
  id: string;
  name: string;
  created_at: string;
}

export interface ApiKeyCreated extends ApiKey {
  key: string;
}

// ---- errors ----

export interface ApiErrorBody {
  error: string;
  detail?: string;
}

// ---- drives ----

export type DriveMode = 'replicated' | 'served';

/** What GET /api/volumes/{drive}/{volume}/inspect reads off the source node. */
export interface VolumeInspection {
  node: string;
  files: number;
  bytes: number;
  /** Unix seconds of the newest file, null when there is none. */
  newest_file_at: number | null;
}

export interface DriveNodeStatus {
  node: string;
  reachable: boolean;
}

export interface ServedConflict {
  holder: string;
  other: string;
  reason: string;
}

export interface ServedStatus {
  holder: string;
  epoch: number;
  handover_to: string | null;
  conflict: ServedConflict | null;
  failback_blocked: string | null;
}

export interface ServedAuthority {
  holder: string;
  epoch: number;
  handover: { to: string; at: number } | null;
  conflict: (ServedConflict & { at: number }) | null;
  blocked: { node: string; reason: string; at: number } | null;
  failback_requested: boolean;
  overwrite: string | null;
}

export type DriveFailback = 'auto' | 'manual';

export interface DriveStatus {
  name: string;
  mode: DriveMode;
  /** Candidate serving nodes for a served drive, in priority order. */
  served_by: string[];
  /** The candidate currently answering requests: the first of `served_by`
   *  that's reachable. `null` for a replicated drive, or when every
   *  candidate is unreachable. */
  active_server: string | null;
  sync_interval: string;
  /** Whether a served drive goes back to a preferred node that returns. */
  failback: DriveFailback;
  /** Authority state once a served drive has failed over; `null` for a
   *  replicated drive or one that never failed over. */
  served: ServedStatus | null;
  /** Nodes that implement this drive. */
  nodes: DriveNodeStatus[];
}

// ---- secrets ----

export type SecretCharset = 'alphanumeric' | 'hex' | 'base64' | 'printable';

export type SecretPutRequest =
  | { value: string }
  | { content: string }
  | { generate: { length: number; charset: SecretCharset } };

/** Whether a container runs a secret's current revision. `unknown` means
 *  nothing is recorded (it predates revision tracking, or its node has not
 *  reported yet). */
export type SecretFreshness = 'current' | 'stale' | 'unknown';

/** One instance that references a secret. */
export interface SecretUse {
  instance: InstanceId;
  stack: string;
  service: string;
  node: string;
  delivery: 'file' | 'env';
  /** The environment variable name, or the path inside the container. */
  target: string;
  running: boolean;
  /** The revision the container was created with, when recorded. */
  running_revision: number | null;
  freshness: SecretFreshness;
}

/** A row of `GET /api/secrets`. Never carries a value. */
export interface SecretSummary {
  name: string;
  revision: number;
  owner: string | null;
  key_fingerprint: string;
  used_by: SecretUse[];
  /** How many of `used_by` run an older revision. */
  stale: number;
}

export type SecretRotateRequest = SecretPutRequest & {
  /** Restart the instances that run an older revision, one at a time per service. */
  restart?: boolean;
};

export interface SecretRollStarted {
  revision: number;
  restarting: InstanceId[];
  skipped: { instance: InstanceId; reason: string }[];
}

export interface ClusterKeyStatus {
  fingerprint: string | null;
  held: boolean;
}

export interface TlsFingerprint {
  fingerprint: string;
}

// ---- config ----

export type NetworkKind = 'internal' | 'private' | 'public' | 'egress' | 'wireguard' | 'nat' | 'container';

export interface OpenPort {
  port: number;
  protocol: PortProtocol;
}

export interface PublicAddress {
  ipv4: string | null;
  ipv6: string | null;
  domain: string | null;
  open_ports: OpenPort[];
}

/** Only meaningful on an `egress`-kind network: a second interface to route
 *  out of, and the address used to probe whether the primary interface still
 *  reaches the internet. */
export interface EgressFallback {
  interface: string;
  health_check_ip: string;
}

/** An egress or wireguard network's host interface address. Serializes as a
 *  bare string when `open_ports` is empty and `fallback` is unset. */
export interface InterfaceAddress {
  interface: string;
  open_ports: OpenPort[];
  /** Absent when unset — the server omits the key entirely rather than
   *  sending `null`. */
  fallback?: EgressFallback | null;
}

export type NodeAddress = string | PublicAddress | InterfaceAddress;

/** An open port on a `container` network: the router side reaches
 *  `port` on the host, forwarded to `target_port` (absent = same as `port`). */
export interface ContainerOpenPort {
  port: number;
  protocol: PortProtocol;
  target_port?: number | null;
}

/** Only meaningful on a `container`-kind network: traffic to the network's `subnet` is
 *  routed through the `service` container of `stack`. */
export interface ContainerNetwork {
  stack: string;
  service: string;
  /** Absent = empty. Nodes whose host-level traffic to the network's `subnet` uses the router. */
  hosts?: string[];
  /** Absent = empty. */
  open_ports?: ContainerOpenPort[];
  /** Let the router reach the reverse proxy on 80/443. Absent = false. */
  proxy?: boolean;
}

export interface NetworkConfig {
  type: NetworkKind;
  subnet: string | null;
  domain: string | null;
  /** Publish this network's hostnames to the DNS provider (Cloudflare).
   *  Default true; omitted when true. Only meaningful for internal/private. */
  publish_dns?: boolean;
  /** Present iff `type` is `container`. */
  container?: ContainerNetwork | null;
  /** Networks this one may reach (internal, private and container networks
   *  only carry it). Absent = unrestricted at the host firewall and only its
   *  own network plus public networks at the proxy; `[]` or a list turns
   *  enforcement on. A network always reaches itself and public networks are
   *  implicitly reachable. */
  access?: string[] | null;
}

export type DriveModeConfig = 'replicated' | 'served';

export interface DriveConfig {
  sync_interval: string;
  mode: DriveModeConfig;
  /** Candidate serving nodes for a served drive, in priority order. */
  served_by: string[];
  /** Served drives only; omitted when `auto` (the default). */
  failback?: DriveFailback;
}

export type NodeRoleConfig = 'manager' | 'worker';

export interface NodeConfig {
  // Identity for raft membership and node removal, minted by `clouduler
  // init` when the node is provisioned. Opaque to the ui, just has to
  // round-trip on save.
  id: string;
  role: NodeRoleConfig;
  network: Record<string, NodeAddress>;
  drives: Record<string, string>;
  rpc_port: number;
  data_dir: string | null;
  // Where this node's docker daemon stores images and containers
  // (dockerd's data-root). Null leaves docker's own default alone.
  docker_data_dir: string | null;
  // Overrides secrets.generate_if_missing for this node. Null inherits it.
  // A node that joined an existing cluster has this false so it cannot invent a
  // second cluster key, so it has to survive an edit here.
  generate_secret_key: boolean | null;
  // Only host an instance when no other eligible node can take it. For a
  // node whose one virtue is a large drive but weak cpu/ram, this keeps the
  // scheduler from piling general workload onto it just because it sorts
  // first.
  last_resort: boolean;
  // Time a durable write for the connectivity table. Absent means on.
  disk_probe?: boolean;
}

export interface WebConfig {
  address: string;
  network: string;
  port: number;
  through_proxy: boolean;
  backend_port: number;
}

export interface FailoverConfig {
  node_timeout: string;
  fence_timeout: string;
  probe_interval: string;
  stop_grace: string;
}

export interface SecretsConfig {
  key_file: string;
  generate_if_missing: boolean;
  /** Path to this node's own asymmetric keypair, generated on first launch. */
  node_key_file: string;
}

export interface OidcConfig {
  issuer: string;
  client_id: string;
  scopes: string[];
  admin_claim: string;
  admin_value: string | null;
  username_claim: string;
}

export interface AuthConfig {
  oidc: OidcConfig | null;
  api_keys: boolean;
  session_ttl: string;
}

export interface CloudflareConfig {
  zones: string[];
}

/** GET /api/cloudflare/token. The token value itself is never returned. */
export interface CloudflareTokenStatus {
  configured: boolean;
}

/** GET /api/oidc/client-secret. The secret value itself is never returned. */
export interface OidcClientSecretStatus {
  configured: boolean;
}

export interface AcmeConfig {
  enabled: boolean;
  directory_url?: string | null;
  contact_email?: string | null;
}

/** Who a proxied hostname's DNS record names. */
export type DnsPublishMode = "all-nodes" | "owning-node";

export type PortalPalette = 'grape' | 'sunset' | 'ocean' | 'forest';

/**
 * `proxy.auth`: the "service login" that protects apps carrying the
 * `authenticated` route middleware. Separate from the dashboard sign-in
 * (`auth.oidc`). Absent or null means the feature is off.
 */
export interface ProxyAuthConfig {
  /** Label, qualified with each listed network's domain. */
  host: string;
  /** Networks that offer the login; one portal per network. */
  networks: string[];
  issuer: string;
  client_id: string;
  scopes: string[];
  groups_claim: string;
  username_claim: string;
  session_ttl: string;
  title: string;
  palette: PortalPalette;
  accent?: string | null;
  /** Networks (a subset of `networks`) whose login page offers "remember this
   *  IP address". Absent or empty means none. */
  remember_ip?: string[];
  /** How long a saved address skips the login, e.g. `30d`. */
  remember_ip_ttl?: string;
  /** Publish the portal host with Cloudflare's proxy on (public networks in `networks`). */
  cloudflare_proxy?: boolean;
}

/** One row of GET /api/proxy/remembered-ips (admin only). */
export interface RememberedIp {
  id: string;
  network: string;
  ip: string;
  username: string;
  groups: string[];
  /** Unix seconds. */
  expires_at: number;
}

/** `proxy.dns_server`: the cluster's own DNS answerer. */
export interface DnsServerConfig {
  enabled: boolean;
  /** Empty means every internal/private network the node has an address on. */
  networks: string[];
  port: number;
  /** Resolvers for names the cluster does not own, in order. Empty means the
   *  host's resolvers without Tailscale MagicDNS. */
  upstream?: string[];
}

export interface ProxyConfig {
  enabled: boolean;
  http_port: number;
  https_port: number;
  acme: AcmeConfig;
  dns_publish: DnsPublishMode;
  dns_server?: DnsServerConfig;
  auth?: ProxyAuthConfig | null;
}

/** A UI-managed redirect. Not part of cluster.yaml; see /api/redirects. */
export interface RedirectRule {
  id: string;
  host: string;
  network: string;
  path_prefix?: string | null;
  target: string;
  preserve_path: boolean;
  status: 301 | 302 | 307 | 308;
  /** Publish the host's DNS records with Cloudflare's proxy on. Public networks only. */
  cloudflare_proxy?: boolean;
}

/**
 * A UI-managed static/external proxy backend: a route for a service
 * clouduler does not run itself, backed by a fixed address instead of a
 * stack's containers. Not part of cluster.yaml; see /api/static-routes.
 */
export interface StaticRouteRule {
  id: string;
  host: string;
  network: string;
  /** Node DNS publishes for this host; unset publishes every node. */
  node?: string | null;
  /** `host:port`, resolved fresh on every reconcile pass. */
  backend: string;
  path_prefix?: string | null;
  tls: boolean;
  max_body_size?: string | null;
  websockets: boolean;
  preserve_host: boolean;
  response_timeout?: string | null;
  /** Publish the host's DNS records with Cloudflare's proxy on. Public networks only. */
  cloudflare_proxy?: boolean;
}

/**
 * A UI-managed rule letting a port in through a wireguard network's tunnel
 * and forwarding it to a port on this node's own address, where an
 * already-published docker port bind picks it up. Not part of cluster.yaml;
 * see /api/wireguard-ingress.
 */
export interface WireguardIngressRule {
  id: string;
  /** Must name a `wireguard`-kind network. */
  network: string;
  port: number;
  protocol: 'tcp' | 'udp';
  /** Port on this node's own docker port bind. Equal to `port` when no renumbering is needed. */
  target_port: number;
}

export interface ClusterConfig {
  cluster: { name: string };
  networks: Record<string, NetworkConfig>;
  drives: Record<string, DriveConfig>;
  nodes: Record<string, NodeConfig>;
  web: WebConfig;
  data_dir: string;
  failover: FailoverConfig;
  secrets: SecretsConfig;
  auth: AuthConfig;
  cloudflare: CloudflareConfig | null;
  proxy: ProxyConfig;
  self_update: SelfUpdateConfig | null;
  backup: GitBackupConfig | null;
  mcp: McpConfig;
  updates?: ImageUpdatesConfig;
}

export interface ImageUpdatesConfig {
  /** How often the leader asks the registries about declared images, e.g. `6h`. */
  check_interval: string;
}

export interface McpConfig {
  enabled: boolean;
}

// ---- validation ----

export interface ValidationError {
  path: string;
  message: string;
}

export interface ValidationResult {
  valid: boolean;
  errors: ValidationError[];
}

export interface StackValidationResult extends ValidationResult {
  requirements: StackRequirements | null;
  /** The same breakdown as `requirements`, one entry per service — used to
   *  show a separate placement table per container. */
  per_service: Record<string, StackRequirements>;
}

// ---- stats ----

export interface InstanceStatsSample {
  cpu_percent: number;
  memory_used_bytes: number;
  memory_limit_bytes: number;
  network_rx_bytes: number;
  network_tx_bytes: number;
  io_read_bytes: number;
  io_write_bytes: number;
}

export interface LogLine {
  timestamp: string;
  stream: 'stdout' | 'stderr';
  message: string;
}

// ---- self-update ----

/** GitHub-release-based cluster self-update. Write-once through the ui, like
 *  `cloudflare`. */
export interface SelfUpdateConfig {
  release_url: string;
  /** Whether the leader periodically looks up the latest release. */
  check_updates?: boolean;
  /** How often, as a duration such as `24h`. */
  check_interval?: string;
}

/** The leader's last periodic release check, stored in the reserved config
 *  `_clouduler_self_update_check` as base64 json. */
export interface SelfUpdateCheck {
  checked_at: number;
  tag: string | null;
  error: string | null;
}

/** GET /api/self-update/github-token. The token itself is write-only, like
 *  the Cloudflare token: only its presence is ever reported back. */
export interface SelfUpdateGithubTokenStatus {
  configured: boolean;
}

/** One node's progress through a self-update run, forward-only. Matches
 *  `cl_update::plan::NodePhase`: a unit variant serializes as a bare string,
 *  a variant carrying data as `{ variant_name: { ...fields } }`. */
export type SelfUpdateNodePhase =
  | 'idle'
  | 'fetching'
  | { fetch_failed: { reason: string } }
  | 'ready'
  | 'restarting'
  | 'done'
  | { restart_failed: { reason: string } };

/** One asset resolved for one architecture. Matches `cl_update::release::ReleaseAsset`. */
export interface SelfUpdateReleaseAsset {
  id: number;
  name: string;
}

/** Stored in `ClusterState.configs['_clouduler_self_update_run']`, so it
 *  arrives live over the same `/api/events` stream as everything else. */
export interface SelfUpdateRunState {
  id: string;
  release_tag: string | null;
  owner: string | null;
  repo: string | null;
  /** arch -> the asset resolved for it */
  assets: Record<string, SelfUpdateReleaseAsset>;
  /** arch -> blake3 hash of the downloaded binary */
  arch_hashes: Record<string, string>;
  /** node name -> its current phase */
  per_node: Record<string, SelfUpdateNodePhase>;
  started_at: number;
  finished_at: number | null;
  failure_reason: string | null;
  restart_started_at: number | null;
}

// ---- git backup ----

/** Where and how the cluster mirrors its own state into git. Freely
 *  editable through `PUT /api/config`, unlike `self_update`. Matches
 *  `cl_config::GitBackupConfig`. */
export interface GitBackupConfig {
  repo_url: string;
  age_recipient: string;
  branch: string;
}

/** Stored in `ClusterState.configs['_clouduler_git_backup_last_export']`, so
 *  it arrives live over the same `/api/events` stream as everything else.
 *  Matches `cl_backup::status::LastExport`. */
export interface GitBackupLastExport {
  /** Unix seconds. */
  at: number;
  summary: string;
  /** The git commit that was pushed, absent for a no-op tick. */
  commit: string | null;
  /** Set when the export attempt failed; the task keeps retrying regardless. */
  error: string | null;
}

/** Preserved on `RejectedPull.preserved_branch` when a pulled commit fails
 *  validation. Matches `cl_backup::status::RejectedPull`. */
export interface RejectedPull {
  reason: string;
  preserved_branch: string;
}

/** Stored in `ClusterState.configs['_clouduler_git_backup_last_pull']`, live
 *  over `/api/events` like `GitBackupLastExport`. Matches
 *  `cl_backup::status::LastPull`. */
export interface GitBackupLastPull {
  /** Unix seconds. */
  at: number;
  /** The commit that was applied, absent when the pull was rejected or a no-op. */
  commit: string | null;
  rejected: RejectedPull | null;
}

/** GET /api/git-backup/status. */
export interface GitBackupStatus {
  configured: boolean;
  /** The generated deploy key's public half, once the leader has made one -
   *  hand this to the git host as a write-capable deploy key. */
  deploy_public_key: string | null;
  last_export: GitBackupLastExport | null;
  last_pull: GitBackupLastPull | null;
}

/** GET /api/git-backup/webhook-secret. The token itself is write-only, like
 *  the self-update github token. */
export interface GitBackupWebhookSecretStatus {
  configured: boolean;
}

export type AutoUpdateOutcome = 'waiting' | 'queued' | 'updated' | 'failed' | 'notified';

/** Mirrors `cl_api::routes::updates::AutoUpdateRow`. */
export interface AutoUpdateRow {
  mode: 'apply' | 'notify';
  delay_secs: number;
  /** Absent until the newest digest has been recorded. */
  outcome?: AutoUpdateOutcome;
  /** Unix seconds the newest digest was first seen. */
  detected_at?: number;
  /** Unix seconds a waiting apply will be queued. */
  apply_at?: number;
  error?: string;
}

/** One row of the Updates view: what a container runs versus what its
 *  registry currently serves. Mirrors `cl_api::routes::updates::UpdateRow`. */
export interface UpdateRow {
  id: InstanceId;
  stack: string;
  service: string;
  node: string;
  /** The reference the container was created from, e.g. `nginx:1.25`. */
  image: string | null;
  /** Registry digest of the bits actually running. */
  running_digest: string | null;
  /** What the registry serves for `image` right now. */
  latest_digest: string | null;
  /** What the cluster has pinned, if anything. */
  pinned_digest: string | null;
  /** The registry serves something other than what is running. */
  update_available: boolean;
  /** A pin exists but the container was never rebuilt onto it. Fixed by
   *  rebuilding, not by pulling — which is why it is not `update_available`. */
  pin_drift: boolean;
  /** Unix seconds the registry was last asked about `image`. The registry is
   *  checked on a timer and by Refresh, never when this list is fetched. */
  checked_at?: number;
  /** `x-clouduler.auto_update` for this instance, present when it is on. */
  auto_update?: AutoUpdateRow;
  /** Why the registry could not be asked. Carried per row so one bad login
   *  does not blank out the rest of the cluster's answer. */
  error?: string;
}

/** What `POST /api/updates/apply` took on. It answers 202 as soon as the work
 *  is scheduled — a pull runs far longer than any http timeout allows — so
 *  this reports what was accepted, not what finished. Progress is watched
 *  through the instance list: each one goes `pulling`, then `running`. */
export interface AcceptedUpdates {
  accepted: InstanceId[];
}

/** Outcome of one instance in a bulk pin call. */
export interface BulkResult {
  id: InstanceId;
  ok: boolean;
  error?: string;
}

/** A stored registry login. The password is write-only: it never comes back
 *  over the api, by construction. */
export interface RegistryAccount {
  server: string;
  username: string;
}

/** Result of running a service action (`POST /api/instances/{id}/actions/{name}`). */
export interface ExecRunResult {
  /** Wall time of the action. Absent on an older daemon. */
  duration_ms?: number;
  stdout: string;
  stderr: string;
  exit_code: number | null;
  timed_out: boolean;
  stdout_truncated: boolean;
  stderr_truncated: boolean;
}

// ---- redesign additions (all optional on the wire until the backend lands) ----

/** One row of GET /api/proxy/routes: what the reverse proxy serves right now. */
export interface ProxyRoute {
  host: string;
  path: string;
  kind: 'service' | 'redirect' | 'static' | 'dashboard';
  stack?: string;
  service?: string;
  port?: number;
  network?: string;
  tls: boolean;
  websockets: boolean;
  /** The host is a zone apex ("@"). */
  apex: boolean;
  /** Unix seconds the serving certificate expires, if any. */
  cert_not_after?: number;
  health?: { node: string; checked_at: string; healthy: boolean; error?: string | null };
}

/** One entry of a node's daemon log (GET /api/nodes/:name/logs, SSE event `log`). */
export interface NodeLogLine {
  /** Monotonic per node; use it to drop duplicates on reconnect. */
  seq: number;
  /** Unix ms. */
  timestamp: number;
  level: 'error' | 'warn' | 'info' | 'debug' | 'trace';
  target: string;
  message: string;
}

/** GET /api/self-update/latest: what the configured release source offers. */
export interface SelfUpdateLatest {
  tag: string;
  architectures: string[];
  source: string;
  running_version: string;
  /** Unix seconds; the answer is cached for 5 minutes. */
  fetched_at: number;
}

/** One row of GET /api/oauth/requests: a pending MCP consent on the answering node. */
export interface PendingOAuthRequest {
  id: string;
  client_name: string;
  redirect_host: string;
  scope: string;
  requested_grant: McpGrant;
  /** Seconds until the request expires. */
  expires_in: number;
  node: string;
}

/** POST /api/nodes/join-command. The key is shown once; revoke it from the
 *  API keys list if the command is not used. */
export interface JoinCommand {
  command: string;
  key_id: string;
  address: string;
  fingerprint: string;
}

/** GET /api/rebalance/last: the most recent rebalance (replicated); null if none. */
export interface LastRebalance extends RebalanceResponse {
  /** Unix seconds. */
  at: number;
}

// ---------------------------------------------------------------- alerts ---

export type AlertSeverity = 'info' | 'warning' | 'critical';
export type AlertState = 'pending' | 'active' | 'resolved' | 'event';

export interface AlertLink {
  label: string;
  /** UI-relative path, e.g. `/stacks/media`. */
  path: string;
}

export interface AlertDelivery {
  destination: string;
  at: number;
  ok: boolean;
  error?: string;
  attempts: number;
  message_id?: string;
}

export interface AlertView {
  id: string;
  event: string;
  category: string;
  severity: AlertSeverity;
  state: AlertState;
  title: string;
  detail: string;
  subject: string;
  stack?: string;
  instance?: string;
  node?: string;
  opened_at: number;
  last_at: number;
  resolved_at?: number;
  count: number;
  escalated_from?: AlertSeverity;
  links: AlertLink[];
  excerpt: string[];
  deliveries: AlertDelivery[];
}

export interface AlertsResponse {
  active: AlertView[];
  recent: AlertView[];
  counts: { critical: number; warning: number; info: number };
}

/** `muted`, `in_app`, or `{ destinations: [...] }` (the Rust enum, snake_case). */
export type AlertRoute = 'muted' | 'in_app' | { destinations: string[] };

export interface AlertEventOverride {
  severity?: AlertSeverity;
  route?: AlertRoute;
}

export interface AlertCatalogEntry {
  key: string;
  category: string;
  title: string;
  severity: AlertSeverity;
  note: string;
  stateful: boolean;
  stack_scoped: boolean;
  override: AlertEventOverride | null;
  effective_severity: AlertSeverity;
  /** Destination names this event goes to by default, `in-app` first. */
  routes: string[];
}

export type DiscordNotify = 'ping' | 'normal' | 'silent';
export type DiscordStyle = 'embed' | 'short';
export type SmtpSecurity = 'starttls' | 'tls' | 'none';

export interface DiscordNotifyLevels {
  critical: DiscordNotify;
  warning: DiscordNotify;
  info: DiscordNotify;
  resolved: DiscordNotify;
}

interface AlertDestinationBase {
  name: string;
  enabled: boolean;
  severities: AlertSeverity[];
  digest: boolean;
  categories?: string[];
  stacks?: string[];
}

export interface DiscordDestination extends AlertDestinationBase {
  kind: 'discord';
  style: DiscordStyle;
  mention?: string;
  notify: DiscordNotifyLevels;
  buttons: boolean;
  edit_resolved?: boolean;
  username?: string;
}
export interface WebhookDestination extends AlertDestinationBase {
  kind: 'webhook';
}
export interface EmailDestination extends AlertDestinationBase {
  kind: 'email';
  to: string[];
}
export type AlertDestination = DiscordDestination | WebhookDestination | EmailDestination;

export interface AlertSmtp {
  host: string;
  port: number;
  security: SmtpSecurity;
  username?: string;
  from: string;
}

export interface AlertThresholds {
  disk_warning_percent: number;
  disk_critical_percent: number;
  memory_percent: number;
  memory_for: string;
  cert_warning_days: number;
  cert_critical_days: number;
}

export interface AlertSettings {
  enabled: boolean;
  /** humantime, e.g. `5m`. */
  warning_debounce: string;
  digest_at: string;
  timezone?: string;
  renotify_on_escalation: boolean;
  notify_resolved: boolean;
  display_pending_warnings?: boolean;
  retention: string;
  public_url?: string;
  destinations: AlertDestination[];
  smtp?: AlertSmtp;
  events: Record<string, AlertEventOverride>;
  thresholds: AlertThresholds;
}

export interface DestinationHealth {
  ok: boolean | null;
  at?: number;
  error?: string;
  consecutive_failures: number;
}

export interface AlertSettingsResponse {
  settings: AlertSettings;
  /** Per destination: the saved URL cut to its domain (`https://discord.com/…`),
   *  never the whole thing, and whether an Authorization header is saved. */
  secrets: Record<string, { url?: string | null; auth_set: boolean }>;
  smtp_password: boolean;
  health: Record<string, DestinationHealth>;
}

/** Secret edits: absent keeps, null deletes, a string replaces. */
export interface AlertSettingsUpdate {
  settings: AlertSettings;
  secrets?: Record<string, { url?: string | null; auth?: string | null }>;
  smtp_password?: string | null;
}

export type AlertTestRequest =
  | { name: string }
  | {
      destination: AlertDestination;
      secrets: { url?: string; auth?: string };
      smtp?: AlertSmtp;
      smtp_password?: string;
    };

export interface AlertTestResult {
  ok: boolean;
  error?: string;
}

export type ContainerRouterState = 'missing' | 'down' | 'up';

export interface ContainerRouterStatus {
  stack: string;
  service: string;
  instance: string;
  node: string | null;
  ip: string | null;
  running: boolean;
  state: ContainerRouterState;
}

/** GET /api/networks/{name}/status. `router` is null for any other kind. */
export interface NetworkStatus {
  router: ContainerRouterStatus | null;
  /** Each node's address on a container network's shared segment. */
  nodes: Record<string, string>;
}
