// The mutable in-memory world the mock API serves. A scenario builds one of
// these; routes read and mutate it; `commit()` bumps the state version and
// wakes every open /api/events stream, exactly like a raft write would.

import type {
  AlertCatalogEntry,
  AlertSettingsResponse,
  AlertsResponse,
  ApiKey,
  BackupDestinationSummary,
  BackupRun,
  ClusterConfig,
  ClusterKeyStatus,
  ClusterState,
  Connectivity,
  GitBackupStatus,
  Identity,
  InstanceState,
  LastRebalance,
  McpClient,
  NodeState,
  PendingConfig,
  PendingOAuthRequest,
  ProxyRoute,
  RedirectRule,
  RegistryAccount,
  ResourceUsage,
  RestoreRequest,
  SelfUpdateConfig,
  SelfUpdateLatest,
  SelfUpdateRunState,
  SnapshotInfo,
  StaticRouteRule,
  UpdateRow,
  WireguardIngressRule,
} from '../api/types';

/** Fields the server puts in the state body that the ui does not read, so
 *  `ClusterState` does not declare them. The contract test checks they exist. */
export interface ServerOnlyStateFields {
  alert_destinations: Record<string, unknown>;
  alert_inbox: Record<string, unknown>;
  alerts: Record<string, unknown>;
  container_status: Record<string, unknown>;
  crash_moves: Record<string, number>;
  previous_image_digests: Record<string, string>;
  volume_removals: Record<string, unknown>;
}

/** `NodeState` as the server sends it: with the raft node id the ui ignores. */
export type MockNodeState = NodeState & { id: string; last_resort: boolean };

export type MockClusterState = Omit<ClusterState, 'nodes'> &
  ServerOnlyStateFields & { nodes: Record<string, MockNodeState> };

export interface MockDb {
  scenario: string;
  description: string;
  identity: Identity;
  state: MockClusterState;
  config: ClusterConfig;
  /** Per node, what its own sampler reports. Unreachable nodes have none. */
  usage: Record<string, ResourceUsage>;
  /** Container-level overrides on top of what an assignment implies. */
  live: Record<string, Partial<InstanceState>>;
  /** Services the cluster wants but has no node for: not assignments yet. */
  pendingInstances: InstanceState[];
  updates: UpdateRow[];
  proxyRoutes: ProxyRoute[];
  redirects: RedirectRule[];
  staticRoutes: StaticRouteRule[];
  wireguardIngress: WireguardIngressRule[];
  registries: RegistryAccount[];
  apiKeys: ApiKey[];
  mcpClients: McpClient[];
  oauthRequests: PendingOAuthRequest[];
  alerts: AlertsResponse;
  alertCatalog: AlertCatalogEntry[];
  alertSettings: AlertSettingsResponse;
  backupDestinations: BackupDestinationSummary[];
  backupRuns: BackupRun[];
  snapshots: SnapshotInfo[];
  restoreRequests: RestoreRequest[];
  selfUpdate: {
    repo: SelfUpdateConfig | null;
    latest: SelfUpdateLatest;
    run: SelfUpdateRunState | null;
    githubTokenSet: boolean;
  };
  gitBackup: GitBackupStatus;
  gitWebhookSecretSet: boolean;
  cloudflareTokenSet: boolean;
  oidcSecretSet: boolean;
  proxyAuthSecretSet: boolean;
  clusterKey: ClusterKeyStatus;
  pending: PendingConfig;
  lastRebalance: LastRebalance | null;
  connectivity: Connectivity;
  tlsFingerprint: string;
  /** Wall clock the scenario was built at, unix seconds. */
  now: number;
  listeners: Set<() => void>;
  nextId: number;
}

export function commit(db: MockDb) {
  db.state.version += 1;
  for (const fn of [...db.listeners]) fn();
}

export function newId(db: MockDb, prefix: string): string {
  db.nextId += 1;
  return `${prefix}-${db.nextId.toString(16).padStart(6, '0')}`;
}

export class HttpError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

/** The /api/instances body: each assignment plus whatever the container is doing. */
export function listInstances(db: MockDb): InstanceState[] {
  const down = new Set(db.state.unreachable);
  const rows = Object.values(db.state.assignments).map((a): InstanceState => {
    const base: InstanceState = down.has(a.node)
      ? {
          id: a.instance,
          node: a.node,
          desired: a.desired,
          state: 'unknown',
          running: null,
          health: 'none',
          image: null,
          image_digest: a.image_digest,
          started_at: null,
          migration: null,
          restart_count: 0,
          crash_moves: 0,
        }
      : {
          id: a.instance,
          node: a.node,
          desired: a.desired,
          state: a.desired === 'running' ? 'running' : 'stopped',
          running: a.desired === 'running',
          health: a.desired === 'running' ? 'healthy' : 'none',
          image: null,
          image_digest: a.image_digest,
          started_at: null,
          migration: null,
          restart_count: 0,
          crash_moves: 0,
        };
    if (a.migration) {
      base.migration = {
        to: a.migration.to,
        phase: typeof a.migration.phase === 'string' ? a.migration.phase : 'failed',
      };
    }
    return { ...base, ...db.live[a.instance] };
  });
  return [...rows, ...db.pendingInstances].sort((x, y) => x.id.localeCompare(y.id));
}
