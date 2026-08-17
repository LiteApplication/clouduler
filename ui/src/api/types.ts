// Types mirror the JSON the Rust side emits. Field names match the Rust
// struct fields (serde uses them as-is, no camelCase conversion), and enum
// values match `#[serde(rename_all = "lowercase")]`. Keep this file the one
// place that knows the wire shape; nothing else should guess at it.

export type Role = 'manager' | 'worker';
export type Availability = 'active' | 'drain' | 'decommissioned';

export interface NodeState {
  name: string;
  role: Role;
  rpc_addr: string;
  /** drive name -> local backing path */
  drives: Record<string, string>;
  availability: Availability;
}

/** Live resource usage, only present on GET /api/nodes/:name. */
export interface NodeDetail extends NodeState {
  reachable: boolean;
  cpu_count: number;
  load_avg_1: number;
  mem_total_bytes: number;
  mem_used_bytes: number;
  uptime_secs: number;
  container_count: number;
  disks: NodeDiskUsage[];
}

export interface NodeDiskUsage {
  drive: string;
  used_bytes: number;
  total_bytes: number;
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

/** Live container state, folded onto an Assignment for GET /api/instances. */
export interface InstanceState extends Assignment {
  status: 'running' | 'starting' | 'stopped' | 'restarting' | 'unhealthy' | 'unknown';
  health: 'healthy' | 'unhealthy' | 'starting' | 'none';
  image: string;
  created_at: string | null;
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
  version: number;
}

// ---- /api/status ----

export interface ClusterStatus {
  leader: string | null;
  term: number;
  quorum: boolean;
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

export interface DriveStatus {
  name: string;
  mode: DriveMode;
  served_by: string | null;
  sync_interval: string;
  /** node name -> whether it implements this drive */
  nodes: Record<string, boolean>;
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

export type NetworkKind = 'internal' | 'private' | 'public';

export interface PublicAddress {
  ipv4: string | null;
  ipv6: string | null;
  domain: string | null;
}

export type NodeAddress = string | PublicAddress;

export interface NetworkConfig {
  type: NetworkKind;
  subnet: string | null;
  domain: string | null;
}

export type DriveModeConfig = 'replicated' | 'served';

export interface DriveConfig {
  sync_interval: string;
  mode: DriveModeConfig;
  served_by: string | null;
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
}

export interface FailoverConfig {
  node_timeout: string;
  fence_timeout: string;
  probe_interval: string;
}

export interface SecretsConfig {
  key_file: string;
  generate_if_missing: boolean;
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
  api_token_file: string | null;
  zones: string[];
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
}

export interface PendingConfig {
  armed: boolean;
  seconds_left: number | null;
  config: ClusterConfig | null;
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
  timestamp: string;
  cpu_percent: number;
  mem_used_bytes: number;
  mem_limit_bytes: number;
  net_rx_bytes: number;
  net_tx_bytes: number;
  block_read_bytes: number;
  block_write_bytes: number;
}

export interface LogLine {
  timestamp: string;
  stream: 'stdout' | 'stderr';
  message: string;
}
