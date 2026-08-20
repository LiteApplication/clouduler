// Every network call in the app goes through here. Nothing else should call
// fetch directly, so the 401 / 503-not-leader / error-shape handling lives
// in exactly one place.

import type {
  ApiErrorBody,
  ApiKey,
  ApiKeyCreated,
  CertificateState,
  CloudflareTokenStatus,
  ClusterConfig,
  ClusterKeyStatus,
  ClusterState,
  ClusterStatus,
  ConfigEntry,
  DnsRecordState,
  DriveStatus,
  Identity,
  InstanceState,
  PendingConfig,
  RedirectRule,
  SecretEntry,
  SecretPutRequest,
  SelfUpdateConfig,
  SelfUpdateRunState,
  SelfUpdateSshKeyStatus,
  StaticRouteRule,
  ValidationResult,
  VolumeState,
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
 * header; callers that care can retry there, everyone else can just show it.
 */
export class NotLeaderError extends Error {
  leaderUrl: string;

  constructor(leaderUrl: string) {
    super(`not the leader, try ${leaderUrl}`);
    this.name = 'NotLeaderError';
    this.leaderUrl = leaderUrl;
  }
}

/** Set once so a 401 anywhere sends the browser to the login screen. */
let onUnauthorized: (() => void) | null = null;
export function setUnauthorizedHandler(fn: (() => void) | null) {
  onUnauthorized = fn;
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    credentials: 'include',
    headers: {
      Accept: 'application/json',
      ...(init?.body ? { 'Content-Type': 'application/json' } : {}),
      ...init?.headers,
    },
    ...init,
  });

  if (res.status === 401) {
    onUnauthorized?.();
    throw new ApiError(401, 'not authenticated');
  }

  if (res.status === 503 && res.headers.has('Location')) {
    throw new NotLeaderError(res.headers.get('Location')!);
  }

  if (!res.ok) {
    let body: ApiErrorBody | null = null;
    try {
      body = await res.json();
    } catch {
      // body wasn't json, fall through to a generic message below
    }
    throw new ApiError(res.status, body?.error ?? res.statusText, body?.detail);
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

  // cluster
  status: () => get<ClusterStatus>('/api/status'),
  state: () => get<ClusterState>('/api/state'),

  // nodes
  listNodes: () => get<ClusterState['nodes']>('/api/nodes'),
  getNode: (name: string) => get<import('./types').NodeDetail>(`/api/nodes/${encodeURIComponent(name)}`),
  addNode: (node: unknown) => post<void>('/api/nodes', node),
  setAvailability: (name: string, availability: 'active' | 'drain') =>
    patch<void>(`/api/nodes/${encodeURIComponent(name)}`, { availability }),
  removeNode: (name: string) => del<void>(`/api/nodes/${encodeURIComponent(name)}`),

  // stacks
  listStacks: () => get<Record<string, import('./types').StackState>>('/api/stacks'),
  getStack: (name: string) => get<import('./types').StackState>(`/api/stacks/${encodeURIComponent(name)}`),
  deployStack: (
    name: string,
    body: { spec: string; configs?: Record<string, string>; secrets?: Record<string, string> },
  ) => put<void>(`/api/stacks/${encodeURIComponent(name)}`, body),
  removeStack: (name: string) => del<void>(`/api/stacks/${encodeURIComponent(name)}`),
  validateStack: (name: string, spec: string) =>
    post<ValidationResult>(`/api/stacks/${encodeURIComponent(name)}/validate`, { spec }),

  // instances
  listInstances: () => get<InstanceState[]>('/api/instances'),
  getInstance: (id: string) => get<InstanceState>(`/api/instances/${encodeURIComponent(id)}`),
  restartInstance: (id: string) => post<void>(`/api/instances/${encodeURIComponent(id)}/restart`),
  moveInstance: (id: string, node: string) => post<void>(`/api/instances/${encodeURIComponent(id)}/move`, { node }),
  setDesired: (id: string, desired: 'running' | 'stopped') =>
    patch<void>(`/api/instances/${encodeURIComponent(id)}`, { desired }),
  updateInstance: (id: string) => post<void>(`/api/instances/${encodeURIComponent(id)}/update`),

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
  // Not part of the generic apply flow above -- it fans out to state that
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
  listDns: () => get<DnsRecordState[]>('/api/dns'),
  reconcileDns: () => post<void>('/api/dns/reconcile'),

  // self-update: generate the deploy key first, then set the repo
  generateSelfUpdateSshKey: () => post<SelfUpdateSshKeyStatus & { public_key: string }>('/api/self-update/ssh-key'),
  selfUpdateSshKey: () => get<SelfUpdateSshKeyStatus>('/api/self-update/ssh-key'),
  setSelfUpdateRepo: (body: SelfUpdateConfig) => put<void>('/api/self-update/repo', body),
  selfUpdateRepo: () => get<SelfUpdateConfig | null>('/api/self-update/repo'),
  startSelfUpdate: () => post<SelfUpdateRunState>('/api/self-update/run'),
  selfUpdateRun: () => get<SelfUpdateRunState | null>('/api/self-update/run'),
  cancelSelfUpdate: () => post<void>('/api/self-update/cancel'),
};

/**
 * Build a same-origin SSE url. EventSource can't take custom headers, so auth
 * rides the session cookie — which is why an api key is exchanged for one at
 * login rather than being attached per request.
 */
export function sseUrl(path: string): string {
  return path;
}
