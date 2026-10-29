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

/** GET /api/nodes/metrics: node name -> usage, unreachable nodes skipped. */
export type NodeMetrics = Record<string, ResourceUsage>;

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
  /** A tcp connect from the reporting node to the peer's raft rpc port worked. */
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
  status?: 'issued' | 'pending' | 'error';
  error?: string;
  error_at?: number;
}

export interface DnsRecordState {
  fqdn: string;
  record_type: string;
  content: string;
  zone: string;
  provider_id: string | null;
  status?: 'published' | 'pending' | 'error';
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
  /** Nodes that implement this drive. */
  nodes: DriveNodeStatus[];
}

// ---- secrets ----

export type SecretCharset = 'alphanumeric' | 'hex' | 'base64' | 'printable';

export type SecretPutRequest =
  | { value: string }
  | { content: string }
  | { generate: { length: number; charset: SecretCharset } };

export interface ClusterKeyStatus {
  fingerprint: string | null;
  held: boolean;
}

export interface TlsFingerprint {
  fingerprint: string;
}

// ---- config ----

export type NetworkKind = 'internal' | 'private' | 'public' | 'egress' | 'wireguard' | 'nat';

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

export interface NetworkConfig {
  type: NetworkKind;
  subnet: string | null;
  domain: string | null;
}

export type DriveModeConfig = 'replicated' | 'served';

export interface DriveConfig {
  sync_interval: string;
  mode: DriveModeConfig;
  /** Candidate serving nodes for a served drive, in priority order. */
  served_by: string[];
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

export interface ProxyConfig {
  enabled: boolean;
  http_port: number;
  https_port: number;
  acme: AcmeConfig;
  dns_publish: DnsPublishMode;
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
}

export interface McpConfig {
  enabled: boolean;
}

export interface PendingConfig {
  armed: boolean;
  seconds_left: number | null;
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
  health?: { ok: boolean; error?: string; checked_at?: string; node?: string };
}

/** One entry of a node's daemon log (GET /api/nodes/:name/logs, SSE event `log`). */
export interface NodeLogLine {
  timestamp: string;
  level: 'error' | 'warn' | 'info' | 'debug' | 'trace';
  target: string;
  message: string;
}

/** GET /api/self-update/latest: what the configured release source offers. */
export interface SelfUpdateLatest {
  tag: string;
  /** The running version, for comparison. */
  current?: string;
  newer: boolean;
  checked_at?: number;
}

/** One row of GET /api/oauth/requests: a pending MCP consent on the answering node. */
export interface PendingOAuthRequest extends OAuthRequestInfo {
  id: string;
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

/** GET /api/rebalance/last: the most recent rebalance, kept on the leader. */
export interface LastRebalance extends RebalanceResponse {
  /** Unix seconds. */
  at: number;
}
