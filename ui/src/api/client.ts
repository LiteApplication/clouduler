// Every network call in the app goes through here. Nothing else should call
// fetch directly, so the 401 / 503-not-leader / error-shape handling lives
// in exactly one place.

import type {
  ExecRunResult,
  ApiErrorBody,
  ApiKey,
  ApiKeyCreated,
  AcceptedUpdates,
  BackupDestinationKind,
  BackupDestinationSummary,
  BackupRun,
  BulkResult,
  CertificateState,
  CloudflareTokenStatus,
  ClusterConfig,
  ClusterKeyStatus,
  ClusterState,
  ClusterStatus,
  ConfigEntry,
  DnsRecordState,
  DriveStatus,
  GitBackupStatus,
  GitBackupWebhookSecretStatus,
  Identity,
  McpClient,
  McpGrant,
  OAuthRequestInfo,
  InstanceState,
  JoinCommand,
  LastRebalance,
  NodeMetrics,
  PendingOAuthRequest,
  ProxyRoute,
  SelfUpdateLatest,
  OidcClientSecretStatus,
  PendingConfig,
  RebalanceResponse,
  RedirectRule,
  RegistryAccount,
  RestoreRequest,
  SecretEntry,
  SecretPutRequest,
  SelfUpdateConfig,
  SelfUpdateGithubTokenStatus,
  SelfUpdateRunState,
  SnapshotInfo,
  StackDetailResponse,
  StackValidationResult,
  StaticRouteRule,
  TlsFingerprint,
  UpdateRow,
  ValidationResult,
  VolumeState,
  VolumeInspection,
  WireguardIngressRule,
} from './types';

/** Thrown for any non-2xx response, carrying the parsed error body when there is one. */
export class ApiError extends Error {
  status: number;
  detail?: string;

  constructor(status: number, message: string, detail?: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.detail = detail;
  }
}

/**
 * Thrown when a request hit a follower. The leader's URL is on the Location
 * header; callers that care can retry there. It extends ApiError so every
 * plain `instanceof ApiError` catch site shows this message instead of
 * falling back to a generic "X failed" with no explanation.
 */
export class NotLeaderError extends ApiError {
  leaderUrl: string;

  constructor(leaderUrl: string) {
    super(503, `not the leader, try ${leaderUrl}`);
    this.name = 'NotLeaderError';
    this.leaderUrl = leaderUrl;
  }
}

/**
 * Config/secret text is stored on the wire as base64. Plain `btoa`/`atob`
 * only handle Latin1 — any pasted em dash, curly quote, or accented
 * character throws `InvalidCharacterError`, a plain DOMException that isn't
 * an ApiError, so it used to blow up deploy/save with no useful message.
 * Go through UTF-8 bytes instead so arbitrary text round-trips.
 */
export function encodeText(text: string): string {
  return btoa(String.fromCharCode(...new TextEncoder().encode(text)));
}
export function decodeText(base64: string): string {
  const bytes = Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
  return new TextDecoder().decode(bytes);
}

/** Set once so a 401 anywhere sends the browser to the login screen. */
let onUnauthorized: (() => void) | null = null;
export function setUnauthorizedHandler(fn: (() => void) | null) {
  onUnauthorized = fn;
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const method = init?.method ?? 'GET';
  let res: Response;
  try {
    res = await fetch(path, {
      credentials: 'include',
      headers: {
        Accept: 'application/json',
        ...(init?.body ? { 'Content-Type': 'application/json' } : {}),
        ...init?.headers,
      },
      ...init,
    });
  } catch (e) {
    // fetch() rejects (rather than resolving with a non-ok response) on
    // network failure, CORS rejection, a dropped connection, etc. That's not
    // an ApiError by construction, so every `instanceof ApiError` catch site
    // in the UI was falling back to a hardcoded, content-free message here.
    // Wrap it so the real reason survives up to whoever is displaying it.
    const reason = e instanceof Error ? e.message : String(e);
    console.error(`[api] ${method} ${path} network error:`, e);
    throw new ApiError(0, `network error calling ${method} ${path}: ${reason}`);
  }

  if (res.status === 401) {
    onUnauthorized?.();
    throw new ApiError(401, 'not authenticated');
  }

  if (res.status === 503 && res.headers.has('Location')) {
    const leaderUrl = res.headers.get('Location')!;
    console.error(`[api] ${method} ${path} hit a follower, leader is ${leaderUrl}`);
    throw new NotLeaderError(leaderUrl);
  }

  if (!res.ok) {
    let body: ApiErrorBody | null = null;
    let bodyText: string | null = null;
    try {
      bodyText = await res.text();
      body = bodyText ? JSON.parse(bodyText) : null;
    } catch {
      // body wasn't json, fall through to a generic message below
    }
    const message = body?.error ?? res.statusText ?? `HTTP ${res.status}`;
    console.error(
      `[api] ${method} ${path} failed: ${res.status} ${message}`,
      body?.detail ? { detail: body.detail } : bodyText ? { body: bodyText } : undefined,
    );
    throw new ApiError(res.status, message, body?.detail);
  }

  // A success with no body is not a failure. 204 has none by definition, and
  // a good part of the api answers a write with a bare 200 and nothing in it
  // (PUT /api/config among them). res.json() throws on an empty body, which
  // surfaced as "apply failed" on requests that had actually succeeded — so
  // read the body as text and only parse when there is something to parse.
  const text = await res.text();
  if (text.length === 0) {
    return undefined as T;
  }
  return JSON.parse(text) as T;
}

function get<T>(path: string): Promise<T> {
  return request<T>(path, { method: 'GET' });
}

function post<T>(path: string, body?: unknown): Promise<T> {
  return request<T>(path, { method: 'POST', body: body !== undefined ? JSON.stringify(body) : undefined });
}

function put<T>(path: string, body: unknown): Promise<T> {
  return request<T>(path, { method: 'PUT', body: JSON.stringify(body) });
}

function patch<T>(path: string, body: unknown): Promise<T> {
  return request<T>(path, { method: 'PATCH', body: JSON.stringify(body) });
}

function del<T>(path: string): Promise<T> {
  return request<T>(path, { method: 'DELETE' });
}

export const api = {
  // auth
  me: () => get<Identity>('/auth/me'),
  loginWithKey: (key: string) => post<Identity>('/auth/login', { key }),
  logout: () => post<void>('/auth/logout'),
  listKeys: () => get<ApiKey[]>('/api/keys'),
  createKey: (name: string) => post<ApiKeyCreated>('/api/keys', { name }),
  deleteKey: (id: string) => del<void>(`/api/keys/${encodeURIComponent(id)}`),

  // mcp / oauth
  listMcpClients: () => get<McpClient[]>('/api/mcp/clients'),
  revokeMcpClient: (id: string) => del<void>(`/api/mcp/clients/${encodeURIComponent(id)}`),
  listOAuthRequests: () => get<PendingOAuthRequest[]>('/api/oauth/requests'),
  getOAuthRequest: (id: string) => get<OAuthRequestInfo>(`/api/oauth/requests/${encodeURIComponent(id)}`),
  approveOAuthRequest: (id: string, grant: McpGrant) =>
    post<{ redirect: string }>(`/api/oauth/requests/${encodeURIComponent(id)}/approve`, { grant }),
  denyOAuthRequest: (id: string) =>
    post<{ redirect: string }>(`/api/oauth/requests/${encodeURIComponent(id)}/deny`),

  // cluster
  status: () => get<ClusterStatus>('/api/status'),
  state: () => get<ClusterState>('/api/state'),

  // nodes
  listNodes: () => get<ClusterState['nodes']>('/api/nodes'),
  nodeMetrics: () => get<NodeMetrics>('/api/nodes/metrics'),
  nodeMetricsHistory: (name: string) =>
    get<import('./types').NodeMetricSample[]>(`/api/nodes/${encodeURIComponent(name)}/metrics/history`),
  nodeConnectivity: () => get<import('./types').Connectivity>('/api/nodes/connectivity'),
  getNode: (name: string) => get<import('./types').NodeDetail>(`/api/nodes/${encodeURIComponent(name)}`),
  // Best-effort autofill for an egress/wireguard network's host interface
  // field — non-docker/non-clouduler-managed interfaces seen on that node.
  getNodeInterfaces: (name: string) => get<string[]>(`/api/nodes/${encodeURIComponent(name)}/interfaces`),
  createJoinCommand: (body: { name: string; role?: 'manager' | 'worker'; address?: string }) =>
    post<JoinCommand>('/api/nodes/join-command', body),
  addNode: (node: unknown) => post<void>('/api/nodes', node),
  setAvailability: (name: string, availability: 'active' | 'drain') =>
    patch<void>(`/api/nodes/${encodeURIComponent(name)}`, { availability }),
  removeNode: (name: string) => del<void>(`/api/nodes/${encodeURIComponent(name)}`),

  // stacks
  getStack: (name: string) => get<StackDetailResponse>(`/api/stacks/${encodeURIComponent(name)}`),
  deployStack: (
    name: string,
    body: { spec: string; configs?: Record<string, string>; secrets?: Record<string, string> },
  ) => put<void>(`/api/stacks/${encodeURIComponent(name)}`, body),
  removeStack: (name: string) => del<void>(`/api/stacks/${encodeURIComponent(name)}`),
  validateStack: (name: string, spec: string) =>
    post<StackValidationResult>(`/api/stacks/${encodeURIComponent(name)}/validate`, { spec }),

  // instances
  listInstances: () => get<InstanceState[]>('/api/instances'),
  getInstance: (id: string) => get<InstanceState>(`/api/instances/${encodeURIComponent(id)}`),
  restartInstance: (id: string) => post<void>(`/api/instances/${encodeURIComponent(id)}/restart`),
  listActions: (id: string) => get<{ name: string; confirm: boolean }[]>(`/api/instances/${encodeURIComponent(id)}/actions`),
  runAction: (id: string, name: string) =>
    post<ExecRunResult>(`/api/instances/${encodeURIComponent(id)}/actions/${encodeURIComponent(name)}`),
  moveInstance: (id: string, node: string) => post<void>(`/api/instances/${encodeURIComponent(id)}/move`, { node }),
  retryMigration: (id: string) => post<void>(`/api/instances/${encodeURIComponent(id)}/migration/retry`),
  dismissMigration: (id: string) => post<void>(`/api/instances/${encodeURIComponent(id)}/migration/dismiss`),
  setDesired: (id: string, desired: 'running' | 'stopped') =>
    patch<void>(`/api/instances/${encodeURIComponent(id)}`, { desired }),
  updateInstance: (id: string) => post<void>(`/api/instances/${encodeURIComponent(id)}/update`),
  // Actively relocates instances to smooth load across nodes. Each move
  // returned already went through the same BeginMigration path a manual
  // move does, so by the time this resolves they are underway, not just
  // planned.
  rebalance: () => post<RebalanceResponse>('/api/rebalance'),
  lastRebalance: () => get<LastRebalance | null>('/api/rebalance/last'),

  // updates: what every container runs versus what its registry now serves.
  // `pinUpdates`/`applyUpdates` with no argument mean "everything this
  // applies to", which is what the view's bulk buttons send.
  listUpdates: () => get<UpdateRow[]>('/api/updates'),
  pinUpdates: (instances?: string[]) => post<BulkResult[]>('/api/updates/pin', { instances: instances ?? [] }),
  // Answers 202 as soon as the work is scheduled: a pull takes far longer
  // than any http timeout allows, so the response says what was accepted and
  // progress is watched through the instance list (`pulling` -> `running`).
  applyUpdates: (instances?: string[]) => post<AcceptedUpdates>('/api/updates/apply', { instances: instances ?? [] }),

  // Admin-only: the path a CI job POSTs to in order to update a stack's containers.
  stackUpdateWebhook: (name: string) =>
    get<{ path: string }>(`/api/stacks/${encodeURIComponent(name)}/update-webhook`),

  regenerateStackUpdateWebhook: (name: string) =>
    post<{ path: string }>(`/api/stacks/${encodeURIComponent(name)}/update-webhook/regenerate`, {}),

  // registry accounts. The password is write-only: there is no getter for it,
  // and `listRegistries` returns only the server and username.
  listRegistries: () => get<RegistryAccount[]>('/api/registries'),
  putRegistry: (body: { server: string; username: string; password: string }) =>
    put<void>('/api/registries', body),
  verifyRegistry: (body: { server: string; username: string; password: string }) =>
    post<void>('/api/registries/verify', body),
  removeRegistry: (server: string) => del<void>(`/api/registries/${encodeURIComponent(server)}`),

  // configs & secrets
  listConfigs: () => get<ConfigEntry[]>('/api/configs'),
  getConfig: (name: string) => get<ConfigEntry>(`/api/configs/${encodeURIComponent(name)}`),
  putConfig: (name: string, content: string) => put<void>(`/api/configs/${encodeURIComponent(name)}`, { content }),
  removeConfig: (name: string) => del<void>(`/api/configs/${encodeURIComponent(name)}`),

  listSecrets: () => get<SecretEntry[]>('/api/secrets'),
  putSecret: (name: string, body: SecretPutRequest) => put<void>(`/api/secrets/${encodeURIComponent(name)}`, body),
  removeSecret: (name: string) => del<void>(`/api/secrets/${encodeURIComponent(name)}`),

  clusterKey: () => get<ClusterKeyStatus>('/api/secrets/key'),
  uploadClusterKey: (key: string) => post<void>('/api/secrets/key', { key }),

  // drives & volumes
  listDrives: () => get<DriveStatus[]>('/api/drives'),
  listVolumes: () => get<VolumeState[]>('/api/volumes'),
  inspectVolume: (drive: string, volume: string, node?: string) =>
    get<VolumeInspection>(
      `/api/volumes/${encodeURIComponent(drive)}/${encodeURIComponent(volume)}/inspect${node ? `?node=${encodeURIComponent(node)}` : ''}`,
    ),
  freezeVolume: (drive: string, volume: string) =>
    post<void>(`/api/volumes/${encodeURIComponent(drive)}/${encodeURIComponent(volume)}/freeze`),
  unfreezeVolume: (drive: string, volume: string) =>
    post<void>(`/api/volumes/${encodeURIComponent(drive)}/${encodeURIComponent(volume)}/unfreeze`),
  deleteVolume: (drive: string, volume: string) =>
    del<void>(`/api/volumes/${encodeURIComponent(drive)}/${encodeURIComponent(volume)}`),
  syncVolume: (drive: string, volume: string) =>
    post<void>(`/api/volumes/${encodeURIComponent(drive)}/${encodeURIComponent(volume)}/sync`),

  // configuration
  getConfiguration: () => get<ClusterConfig>('/api/config'),
  applyConfiguration: (config: ClusterConfig, rollbackAfter?: string) =>
    put<void>('/api/config', { config, rollback_after: rollbackAfter }),
  validateConfiguration: (config: ClusterConfig) => post<ValidationResult>('/api/config/validate', { config }),
  confirmConfiguration: () => post<void>('/api/config/confirm'),
  rollbackConfiguration: () => post<void>('/api/config/rollback'),
  pendingConfiguration: () => get<PendingConfig>('/api/config/pending'),
  // Renames a network everywhere it is referenced: cluster.yaml, every
  // deployed stack, and the redirect/static-route/wireguard-ingress stores.
  // Not part of the generic apply flow above because it fans out to state that
  // flow does not know about.
  renameNetwork: (name: string, newName: string) =>
    post<void>(`/api/config/networks/${encodeURIComponent(name)}/rename`, { new_name: newName }),
  // Renames a drive everywhere it is referenced: cluster.yaml (the drives
  // key and every node's drive-to-path map) and every deployed stack's
  // volume mounts. Not part of the generic apply flow above, same reason as
  // renameNetwork.
  renameDrive: (name: string, newName: string) =>
    post<void>(`/api/config/drives/${encodeURIComponent(name)}/rename`, { new_name: newName }),

  // cloudflare: the token is write-only, so there is no getter for its value
  cloudflareToken: () => get<CloudflareTokenStatus>('/api/cloudflare/token'),
  setCloudflareToken: (token: string) => put<void>('/api/cloudflare/token', { token }),

  // oidc: the client secret is write-only, same reasoning as the cloudflare token
  oidcClientSecret: () => get<OidcClientSecretStatus>('/api/oidc/client-secret'),
  setOidcClientSecret: (clientSecret: string) =>
    put<void>('/api/oidc/client-secret', { client_secret: clientSecret }),
  proxyAuthClientSecret: () => get<OidcClientSecretStatus>('/api/proxy-auth/client-secret'),
  setProxyAuthClientSecret: (clientSecret: string) =>
    put<void>('/api/proxy-auth/client-secret', { client_secret: clientSecret }),

  // redirects: UI-managed, not part of cluster.yaml — see docs/proxy.md
  listRedirects: () => get<RedirectRule[]>('/api/redirects'),
  createRedirect: (body: Omit<RedirectRule, 'id'>) => post<RedirectRule>('/api/redirects', body),
  updateRedirect: (id: string, body: Omit<RedirectRule, 'id'>) =>
    put<RedirectRule>(`/api/redirects/${encodeURIComponent(id)}`, body),
  removeRedirect: (id: string) => del<void>(`/api/redirects/${encodeURIComponent(id)}`),

  // static routes: UI-managed, not part of cluster.yaml — see docs/proxy.md
  listStaticRoutes: () => get<StaticRouteRule[]>('/api/static-routes'),
  createStaticRoute: (body: Omit<StaticRouteRule, 'id'>) => post<StaticRouteRule>('/api/static-routes', body),
  updateStaticRoute: (id: string, body: Omit<StaticRouteRule, 'id'>) =>
    put<StaticRouteRule>(`/api/static-routes/${encodeURIComponent(id)}`, body),
  removeStaticRoute: (id: string) => del<void>(`/api/static-routes/${encodeURIComponent(id)}`),

  // wireguard: config import (sealed like a secret) and the ingress rules
  // that let ports in through the tunnel, both not part of cluster.yaml
  putWireguardConfig: (network: string, node: string, content: string) =>
    put<{ interface: string }>(
      `/api/wireguard/${encodeURIComponent(network)}/${encodeURIComponent(node)}`,
      { content },
    ),
  removeWireguardConfig: (network: string, node: string) =>
    del<void>(`/api/wireguard/${encodeURIComponent(network)}/${encodeURIComponent(node)}`),
  listWireguardIngress: () => get<WireguardIngressRule[]>('/api/wireguard-ingress'),
  createWireguardIngress: (body: Omit<WireguardIngressRule, 'id'>) =>
    post<WireguardIngressRule>('/api/wireguard-ingress', body),
  updateWireguardIngress: (id: string, body: Omit<WireguardIngressRule, 'id'>) =>
    put<WireguardIngressRule>(`/api/wireguard-ingress/${encodeURIComponent(id)}`, body),
  removeWireguardIngress: (id: string) => del<void>(`/api/wireguard-ingress/${encodeURIComponent(id)}`),

  // certificates & dns
  listCertificates: () => get<CertificateState[]>('/api/certificates'),
  renewCertificate: (fqdn: string) => post<void>(`/api/certificates/${encodeURIComponent(fqdn)}/renew`),
  tlsFingerprint: () => get<TlsFingerprint>('/api/tls-fingerprint'),
  listDns: () => get<DnsRecordState[]>('/api/dns'),
  proxyRoutes: () => get<ProxyRoute[]>('/api/proxy/routes'),
  reconcileDns: () => post<void>('/api/dns/reconcile'),

  // self-update: the github token is write-only, so there is no getter for its value
  selfUpdateGithubToken: () => get<SelfUpdateGithubTokenStatus>('/api/self-update/github-token'),
  setSelfUpdateGithubToken: (token: string) => put<void>('/api/self-update/github-token', { token }),
  setSelfUpdateRepo: (body: SelfUpdateConfig) => put<void>('/api/self-update/repo', body),
  selfUpdateLatest: () => get<SelfUpdateLatest>('/api/self-update/latest'),
  selfUpdateRepo: () => get<SelfUpdateConfig | null>('/api/self-update/repo'),
  startSelfUpdate: () => post<SelfUpdateRunState>('/api/self-update/run'),
  selfUpdateRun: () => get<SelfUpdateRunState | null>('/api/self-update/run'),
  cancelSelfUpdate: () => post<void>('/api/self-update/cancel'),

  gitBackupStatus: () => get<GitBackupStatus>('/api/git-backup/status'),
  gitBackupExportNow: () => post<void>('/api/git-backup/export-now'),
  gitBackupPullNow: () => post<void>('/api/git-backup/pull-now'),
  gitBackupReconcileNow: () => post<void>('/api/git-backup/reconcile-now'),
  gitBackupWebhookSecret: () => get<GitBackupWebhookSecretStatus>('/api/git-backup/webhook-secret'),
  setGitBackupWebhookSecret: (token: string) => put<void>('/api/git-backup/webhook-secret', { token }),

  // restic backups: destinations, per-drive schedules, run history, restore.
  // The `id` for a destination/target is inside the request body, not the
  // url - the server accepts either a new id (create) or an existing one
  // (edit) through the same PUT.
  listBackupDestinations: () => get<BackupDestinationSummary[]>('/api/restic/destinations'),
  putBackupDestination: (body: {
    id: string;
    name: string;
    kind: BackupDestinationKind;
    password?: string;
    ssh_private_key?: string;
    s3_access_key_id?: string;
    s3_secret_access_key?: string;
  }) => put<void>('/api/restic/destinations', body),
  removeBackupDestination: (id: string) => del<void>(`/api/restic/destinations/${encodeURIComponent(id)}`),

  putBackupTarget: (body: {
    id: string;
    drive: string;
    destination_id: string;
    enabled: boolean;
    frequency_secs: number;
    time_of_day?: string | null;
    keep_last?: number | null;
    keep_within_secs?: number | null;
    stop_containers?: boolean | null;
  }) => put<void>('/api/restic/targets', body),
  removeBackupTarget: (id: string) => del<void>(`/api/restic/targets/${encodeURIComponent(id)}`),
  runBackupTarget: (id: string) => post<string>(`/api/restic/targets/${encodeURIComponent(id)}/run`),

  listBackupRuns: (target?: string) =>
    get<BackupRun[]>(`/api/restic/runs${target ? `?target=${encodeURIComponent(target)}` : ''}`),
  listSnapshots: (destinationId: string) =>
    get<SnapshotInfo[]>(`/api/restic/destinations/${encodeURIComponent(destinationId)}/snapshots`),

  restoreDrive: (body: { drive: string; destination_id: string; snapshot?: string | null }) =>
    post<string>('/api/restic/restore/drive', body),
  restoreVolume: (body: { drive: string; volume: string; destination_id: string; snapshot?: string | null }) =>
    post<string>('/api/restic/restore/volume', body),
  restoreService: (body: {
    stack: string;
    service: string;
    destination_id?: string | null;
    snapshot?: string | null;
  }) => post<string>('/api/restic/restore/service', body),
  listRestoreRequests: () => get<RestoreRequest[]>('/api/restic/restore-requests'),
};

/**
 * Build a same-origin SSE url. EventSource can't take custom headers, so auth
 * rides the session cookie — which is why an api key is exchanged for one at
 * login rather than being attached per request.
 */
export function sseUrl(path: string): string {
  return path;
}

/** SSE url for a node's daemon log: replays the tail, then follows. Admin only. */
export function nodeLogsUrl(node: string, tail = 200, follow = true): string {
  return sseUrl(`/api/nodes/${encodeURIComponent(node)}/logs?tail=${tail}&follow=${follow}`);
}
