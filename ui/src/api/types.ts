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
}

export interface ResourceUsage {
  cpu_count: number;
  load_1m: number;
  memory_total_bytes: number;
  memory_used_bytes: number;
  uptime_seconds: number;
  /** False (all-zero above) unless the answering node is this node itself. */
  local: boolean;
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
 *  to inspect a container elsewhere, and that is different from "stopped". */
export interface InstanceState {
  id: InstanceId;
  node: string;
  desired: DesiredState;
  state: 'running' | 'stopped' | 'unknown';
  running: boolean | null;
  health: 'healthy' | 'unhealthy' | 'starting' | 'none';
  image: string | null;
  image_digest: string | null;
  started_at: string | null;
  migration: { to: string; phase: string } | null;
}

export interface Attachment {
  content: string;
  secret: boolean;
}

export interface StackState {
  name: string;
  spec: string;
  revision: number;
  files: Record<string, Attachment>;
  configs: string[];
  secrets: string[];
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
}

export interface DnsRecordState {
  fqdn: string;
  record_type: string;
  content: string;
  zone: string;
  provider_id: string | null;
}

export interface VolumeState {
  id: VolumeIdStr;
  generation: number;
  primary: string;
  replicas: Record<string, number>;
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
  unreachable: string[];
  overlay: string | null;
  /** The cluster key, wrapped once per node -- see WrappedKeyEntry. Ciphertext
   *  only; the plaintext key never appears in this state. */
  wrapped_keys: Record<string, WrappedKeyEntry>;
  version: number;
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
  role: UserRole;
}

export interface ApiKey {
  id: string;
  name: string;
  created_at: string;
}

export interface ApiKeyCreated extends ApiKey {
  secret: string;
}

// ---- errors ----

export interface ApiErrorBody {
  error: string;
  detail?: string;
}

// ---- drives ----

export type DriveMode = 'replicated' | 'served';

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

// ---- config ----

export type NetworkKind = 'internal' | 'private' | 'public' | 'egress' | 'wireguard';

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

/** An egress or wireguard network's host interface address. Serializes as a
 *  bare string when `open_ports` is empty. */
export interface InterfaceAddress {
  interface: string;
  open_ports: OpenPort[];
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
  role: NodeRoleConfig;
  network: Record<string, NodeAddress>;
  drives: Record<string, string>;
  rpc_port: number;
  data_dir: string | null;
  // Overrides secrets.generate_if_missing for this node. Null inherits it.
  // A node that joined an existing cluster has this false so it cannot invent a
  // second cluster key, so it has to survive an edit here.
  generate_secret_key: boolean | null;
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
  client_secret_file: string | null;
  scopes: string[];
  admin_claim: string;
  admin_value: string | null;
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

export interface AcmeConfig {
  enabled: boolean;
  directory_url?: string | null;
  contact_email?: string | null;
}

export interface ProxyConfig {
  enabled: boolean;
  http_port: number;
  https_port: number;
  acme: AcmeConfig;
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
