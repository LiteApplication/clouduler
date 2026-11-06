// The fake API surface: one entry per endpoint the real UI client can call
// (see `api` in src/api/client.ts), plus the SSE streams. Handlers read and
// mutate the in-memory MockDb; whatever they return is what the real server's
// JSON would be, which src/mock/contract.test.ts checks against fixtures
// generated from the Rust API.

import { encodeText } from '../api/client';
import type {
  AlertSettingsUpdate,
  BackupDestination,
  BackupDestinationKind,
  BackupRun,
  BulkResult,
  CertificateState,
  ClusterConfig,
  ClusterStatus,
  Connectivity,
  DnsRecordState,
  DriveStatus,
  ExecRunResult,
  InstanceProgressEvent,
  InstanceStatsSample,
  JoinCommand,
  LogLine,
  NodeDetail,
  NodeLogLine,
  NodeMetricSample,
  NodeMetrics,
  SecretPutRequest,
  SecretRollStarted,
  SecretSummary,
  StackDetailResponse,
  StackState,
  StackValidationResult,
  VolumeInspection,
} from '../api/types';
import { commit, HttpError, listInstances, newId, type MockDb } from './db';
import { fakeDigest } from './builders';
import { requirementsFor } from './spec';

export interface RouteCtx {
  db: MockDb;
  params: Record<string, string>;
  query: URLSearchParams;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  body: any;
}

export interface HttpRoute {
  method: 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE';
  pattern: string;
  handler: (ctx: RouteCtx) => unknown;
}

export interface StreamCtx {
  db: MockDb;
  params: Record<string, string>;
  query: URLSearchParams;
  emit: (event: string, data: unknown) => void;
}

export interface StreamRoute {
  pattern: string;
  /** Starts the stream; returns its teardown. */
  start: (ctx: StreamCtx) => () => void;
}

const nowSecs = () => Math.floor(Date.now() / 1000);
const iso = (secs = nowSecs()) => new Date(secs * 1000).toISOString();

function need<T>(v: T | undefined | null, what: string): T {
  if (v === undefined || v === null) throw new HttpError(404, `${what} not found`);
  return v;
}

function domainsOf(db: MockDb): Record<string, string> {
  return Object.fromEntries(Object.entries(db.config.networks).map(([n, c]) => [n, c.domain ?? 'example.com']));
}

function upNodes(db: MockDb): string[] {
  return Object.values(db.state.nodes)
    .filter((n) => n.availability === 'active' && !db.state.unreachable.includes(n.name))
    .map((n) => n.name);
}

function later(db: MockDb, ms: number, fn: () => void) {
  setTimeout(() => {
    fn();
    commit(db);
  }, ms);
}

function clusterStatus(db: MockDb): ClusterStatus {
  const managers = Object.values(db.state.nodes).filter((n) => n.role === 'manager');
  const reachable = managers.filter((n) => !db.state.unreachable.includes(n.name));
  const leader = reachable[0]?.name ?? null;
  return {
    node: leader ?? Object.keys(db.state.nodes)[0] ?? '',
    is_leader: true,
    has_quorum: reachable.length * 2 > managers.length,
    leader,
    voters: managers.length,
    reachable: reachable.length,
    managers_total: managers.length,
    managers_reachable: reachable.length,
    version: '0.9.10',
    state_version: db.state.version,
  };
}

function nodeDetail(db: MockDb, name: string): NodeDetail {
  const n = need(db.state.nodes[name], `node ${name}`);
  const reachable = !db.state.unreachable.includes(name);
  const usage = db.usage[name] ?? {
    cpu_count: 0,
    cpu_percent: 0,
    load_1m: 0,
    local: false,
    memory_total_bytes: 0,
    memory_used_bytes: 0,
    uptime_seconds: 0,
    version: n.version,
    drives: [],
  };
  return {
    name,
    role: n.role,
    availability: n.availability,
    reachable,
    drives: Object.keys(n.drives),
    container_count: Object.values(db.state.assignments).filter((a) => a.node === name).length,
    usage,
  };
}

function nodeMetrics(db: MockDb): NodeMetrics {
  return Object.keys(db.state.nodes)
    .filter((n) => db.usage[n])
    .map((name) => ({
      name,
      container_count: Object.values(db.state.assignments).filter((a) => a.node === name).length,
      usage: db.usage[name],
    }));
}

function metricsHistory(db: MockDb, name: string): NodeMetricSample[] {
  const u = need(db.usage[name], `node ${name}`);
  const now = Date.now();
  return Array.from({ length: 120 }, (_, i) => ({
    at: now - (119 - i) * 5000,
    cpu_percent: Math.max(1, Math.round((u.cpu_percent ?? 10) + Math.sin(i / 6) * 6 + Math.sin(i * 1.7) * 2)),
    memory_used_bytes: Math.round(u.memory_used_bytes * (1 + Math.sin(i / 20) * 0.02)),
  }));
}

function volumeRows(db: MockDb) {
  return Object.values(db.state.volumes).map((v) => ({
    id: v.id,
    generation: v.generation,
    primary: v.primary,
    frozen: v.frozen,
    sync_refused: v.sync_refused,
    looks_empty: v.looks_empty,
    replicas: Object.entries(v.replicas).map(([node, generation]) => ({
      node,
      generation,
      in_sync: generation === v.generation,
    })),
  }));
}

function driveRows(db: MockDb): DriveStatus[] {
  return Object.entries(db.config.drives).map(([name, d]) => {
    const nodes = Object.values(db.state.nodes)
      .filter((n) => name in n.drives)
      .map((n) => ({ node: n.name, reachable: !db.state.unreachable.includes(n.name) }));
    const servedBy = d.served_by ?? [];
    const active =
      d.mode === 'served' ? (servedBy.find((s) => !db.state.unreachable.includes(s)) ?? null) : null;
    return { name, mode: d.mode, served_by: servedBy, active_server: active, sync_interval: d.sync_interval, nodes };
  });
}

function certRows(db: MockDb): (Omit<CertificateState, 'chain_pem' | 'key_pem'> & { has_chain: boolean })[] {
  const rows = Object.values(db.state.certificates).map((c) => ({
    fqdn: c.fqdn,
    has_chain: true,
    issued_at: c.issued_at,
    not_after: c.not_after,
    status: 'ready' as const,
  }));
  for (const [fqdn, st] of Object.entries(db.state.cert_status)) {
    if (st === 'Pending') rows.push({ fqdn, has_chain: false, issued_at: 0, not_after: 0, status: 'pending' as never });
    else
      rows.push({
        fqdn,
        has_chain: false,
        issued_at: 0,
        not_after: 0,
        status: 'error' as never,
        error: st.Error.message,
        error_at: st.Error.at,
      } as never);
  }
  return rows;
}

function dnsRows(db: MockDb): Omit<DnsRecordState, 'provider_id'>[] {
  const rows: Omit<DnsRecordState, 'provider_id'>[] = Object.values(db.state.dns_records).map((r) => ({
    fqdn: r.fqdn,
    record_type: r.record_type,
    content: r.content,
    zone: r.zone,
    status: 'ready',
  }));
  for (const [fqdn, st] of Object.entries(db.state.dns_status)) {
    rows.push({ fqdn, record_type: '', content: '', zone: '', status: st === 'Pending' ? 'pending' : 'error' });
  }
  return rows;
}

/** Re-derive proxy routes, certificates and dns records for a stack's hosts. */
function publishHosts(db: MockDb, stack: string, spec: string) {
  const domains = domainsOf(db);
  const { parsed } = requirementsFor(spec, domains);
  db.proxyRoutes = db.proxyRoutes.filter((r) => r.stack !== stack);
  for (const svc of parsed.services) {
    for (const p of svc.proxy) {
      const host = p.host.includes('.') ? p.host : `${p.host}.${domains[p.network] ?? 'example.com'}`;
      const owner = db.state.assignments[`${stack}.${svc.name}.0`]?.node ?? Object.keys(db.state.nodes)[0];
      const ip = String(Object.values(db.config.nodes[owner]?.network ?? {})[0] ?? '192.0.2.1');
      db.proxyRoutes.push({
        host,
        path: p.path_prefix ?? '/',
        kind: 'service',
        stack,
        service: svc.name,
        port: p.port,
        network: p.network,
        tls: true,
        websockets: true,
        apex: false,
        health: { node: owner, checked_at: iso(), healthy: true, error: null },
      });
      if (!db.state.certificates[host] && !db.state.cert_status[host]) {
        db.state.cert_status[host] = 'Pending';
        later(db, 2500, () => {
          delete db.state.cert_status[host];
          db.state.certificates[host] = {
            fqdn: host,
            chain_pem: '',
            key_pem: '',
            issued_at: nowSecs(),
            not_after: nowSecs() + 86400 * 90,
          };
          const r = db.proxyRoutes.find((x) => x.host === host);
          if (r) r.cert_not_after = nowSecs() + 86400 * 90;
        });
      }
      const key = `${host}|A|${ip}`;
      db.state.dns_records[key] ??= { fqdn: host, record_type: 'A', content: ip, zone: domains[p.network], provider_id: null };
    }
  }
}

function deployStack(
  db: MockDb,
  name: string,
  body: { spec: string; configs?: Record<string, string>; secrets?: Record<string, string> },
) {
  if (typeof body?.spec !== 'string') throw new HttpError(400, 'spec is required');
  const { parsed, requirements } = requirementsFor(body.spec, domainsOf(db));
  if (parsed.services.length === 0) throw new HttpError(400, 'stack has no services');
  const previous = db.state.stacks[name];
  for (const [key, content] of Object.entries(body.configs ?? {})) {
    putConfigEntry(db, key, content, name);
  }
  for (const key of Object.keys(body.secrets ?? {})) putSecretEntry(db, key, name);
  const stack: StackState = {
    name,
    spec: body.spec,
    revision: (previous?.revision ?? 0) + 1,
    configs: Object.keys(body.configs ?? {}),
    secrets: Object.keys(body.secrets ?? {}),
    used_configs: requirements.configs,
    used_secrets: requirements.secrets,
    deployed_at: nowSecs(),
    previous_spec: previous?.spec ?? null,
  };
  db.state.stacks[name] = stack;

  const wanted = new Set<string>();
  const nodes = upNodes(db);
  let rr = Object.keys(db.state.assignments).length;
  for (const svc of parsed.services) {
    for (let i = 0; i < svc.replicas; i++) {
      const id = `${name}.${svc.name}.${i}`;
      wanted.add(id);
      if (db.state.assignments[id]) {
        db.live[id] = { ...db.live[id], image: svc.image, state: 'restarting', reason: 'redeploying' };
        later(db, 1800, () => {
          db.live[id] = { ...db.live[id], state: 'running', running: true, health: 'healthy', reason: null };
        });
        continue;
      }
      const node = svc.node ?? nodes[rr++ % Math.max(nodes.length, 1)];
      if (!node) {
        db.pendingInstances.push({
          id,
          node: '',
          desired: 'running',
          state: 'pending_placement',
          running: null,
          health: 'none',
          image: svc.image,
          image_digest: null,
          started_at: null,
          migration: null,
          placement_error: 'no active node available',
          restart_count: 0,
          crash_moves: 0,
        });
        continue;
      }
      const digest = fakeDigest(svc.image);
      db.state.assignments[id] = { instance: id, node, desired: 'running', image_digest: digest, migration: null };
      db.live[id] = {
        image: svc.image,
        state: 'pulling',
        running: false,
        health: 'none',
        reason: 'pulling image',
        step: [1, 3],
      };
      later(db, 2200 + i * 300, () => {
        db.live[id] = {
          image: svc.image,
          running_digest: digest,
          started_at: iso(),
          exit_code: null,
          oom_killed: false,
          restart_limit: 5,
          memory_limit_bytes: null,
          state: 'running',
          running: true,
          health: 'healthy',
          reason: null,
          step: null,
        };
      });
      for (const vol of svc.volumes) {
        db.state.volumes[vol] ??= {
          id: vol,
          generation: 1,
          primary: node,
          replicas: { [node]: 1 },
          frozen: null,
          sync_refused: null,
          looks_empty: false,
        };
      }
      db.updates.push({
        id,
        stack: name,
        service: svc.name,
        node,
        image: svc.image,
        running_digest: digest,
        latest_digest: digest,
        pinned_digest: null,
        update_available: false,
        pin_drift: false,
      });
    }
  }
  for (const id of Object.keys(db.state.assignments)) {
    if (id.startsWith(`${name}.`) && !wanted.has(id)) dropInstance(db, id);
  }
  publishHosts(db, name, body.spec);
}

function dropInstance(db: MockDb, id: string) {
  delete db.state.assignments[id];
  delete db.live[id];
  db.updates = db.updates.filter((u) => u.id !== id);
}

function putConfigEntry(db: MockDb, name: string, content: string, owner: string | null = null) {
  const prev = db.state.configs[name];
  db.state.configs[name] = { name, content, revision: (prev?.revision ?? 0) + 1, owner: prev?.owner ?? owner };
}

/** Stores a json object the way the server stores a reserved `_clouduler_*` config. */
function putReservedConfig(db: MockDb, name: string, value: unknown) {
  putConfigEntry(db, name, encodeText(JSON.stringify(value)));
}

function putSecretEntry(db: MockDb, name: string, owner: string | null = null) {
  const prev = db.state.secrets[name];
  db.state.secrets[name] = {
    name,
    sealed: '',
    revision: (prev?.revision ?? 0) + 1,
    key_fingerprint: fakeDigest(`secret-${name}`).slice(7, 23),
    owner: prev?.owner ?? owner,
  };
}

/** Secrets whose running containers have not been restarted since the last
 *  overwrite, per mock db. The real server derives this from the revision each
 *  container was created with. */
const staleSecrets = new WeakMap<MockDb, Set<string>>();

function secretSummaries(db: MockDb): SecretSummary[] {
  const stale = staleSecrets.get(db) ?? new Set<string>();
  return Object.values(db.state.secrets).map((secret) => {
    const used_by: SecretSummary['used_by'] = [];
    for (const stack of Object.values(db.state.stacks)) {
      if (!stack.used_secrets.includes(secret.name)) continue;
      for (const a of Object.values(db.state.assignments)) {
        if (a.instance.split('.')[0] !== stack.name) continue;
        const isStale = stale.has(secret.name);
        used_by.push({
          instance: a.instance,
          stack: stack.name,
          service: a.instance.split('.')[1] ?? '',
          node: a.node,
          delivery: 'file',
          target: `/run/secrets/${secret.name}`,
          running: a.desired === 'running',
          running_revision: isStale ? Math.max(1, secret.revision - 1) : secret.revision,
          freshness: isStale ? 'stale' : 'current',
        });
      }
    }
    return { ...secret, used_by, stale: used_by.filter((u) => u.freshness === 'stale').length };
  });
}

function rollSecretUsers(db: MockDb, name: string): SecretRollStarted {
  const restarting = secretSummaries(db)
    .find((s) => s.name === name)
    ?.used_by.filter((u) => u.freshness === 'stale' && u.running)
    .map((u) => u.instance) ?? [];
  staleSecrets.get(db)?.delete(name);
  return { revision: db.state.secrets[name]?.revision ?? 0, restarting, skipped: [] };
}

function validateStack(db: MockDb, spec: string): StackValidationResult {
  const { parsed, requirements, per_service } = requirementsFor(String(spec ?? ''), domainsOf(db));
  const errors: StackValidationResult['errors'] = [];
  if (parsed.services.length === 0) errors.push({ path: 'services', message: 'a stack needs at least one service' });
  for (const s of parsed.services) {
    if (!s.image) errors.push({ path: `services.${s.name}.image`, message: 'image is required' });
    for (const n of s.networks) {
      if (!db.config.networks[n]) errors.push({ path: `services.${s.name}.networks`, message: `network ${n} is not declared in the cluster` });
    }
    for (const v of s.volumes) {
      if (!db.config.drives[v.split('/')[0]]) errors.push({ path: `services.${s.name}.volumes`, message: `drive ${v.split('/')[0]} is not declared in the cluster` });
    }
  }
  return { valid: errors.length === 0, errors, requirements: errors.length === 0 ? requirements : null, per_service };
}

function maskUrl(url: string): string {
  try {
    return `${new URL(url).origin}/…`;
  } catch {
    return '…';
  }
}

function renameInSpec(spec: string, from: string, to: string, kind: 'network' | 'drive'): string {
  const esc = from.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  if (kind === 'drive') return spec.replace(new RegExp(`(^|[\\s-])${esc}/`, 'gm'), `$1${to}/`);
  return spec.replace(new RegExp(`(network:\\s*|\\[|,\\s*|^\\s*-\\s+|^\\s*)${esc}(?=\\s*[:,\\]]|\\s*$)`, 'gm'), `$1${to}`);
}

function renameKey<T>(obj: Record<string, T>, from: string, to: string) {
  if (!(from in obj)) throw new HttpError(404, `${from} not found`);
  const entries = Object.entries(obj).map(([k, v]) => [k === from ? to : k, v] as const);
  for (const k of Object.keys(obj)) delete obj[k];
  for (const [k, v] of entries) obj[k] = v;
}

const route = (method: HttpRoute['method'], pattern: string, handler: HttpRoute['handler']): HttpRoute => ({
  method,
  pattern,
  handler,
});

const sampleRun = (db: MockDb, targetId: string, drive: string, dest: string): BackupRun => ({
  id: newId(db, 'run'),
  target_id: targetId,
  drive,
  destination_id: dest,
  started_at: nowSecs(),
  finished_at: nowSecs() + 42,
  outcome: { Succeeded: { snapshot_id: fakeDigest(targetId).slice(7, 15), bytes: 1_840_000_000 } },
  triggered_by: { Manual: { by: 'demo' } },
});

export function buildHttpRoutes(): HttpRoute[] {
  return [
    // ---- alerts
    route('GET', '/api/alerts', ({ db }) => db.alerts),
    route('GET', '/api/alerts/catalog', ({ db }) => db.alertCatalog),
    route('GET', '/api/alerts/settings', ({ db }) => db.alertSettings),
    route('PUT', '/api/alerts/settings', ({ db, body }) => {
      const upd = body as AlertSettingsUpdate;
      if (!upd?.settings) throw new HttpError(400, 'settings required');
      const cur = db.alertSettings;
      cur.settings = upd.settings;
      for (const d of upd.settings.destinations) {
        const edit = upd.secrets?.[d.name];
        const prev = cur.secrets[d.name] ?? { url: null, auth_set: false };
        cur.secrets[d.name] = {
          url: edit && 'url' in edit ? (edit.url ? maskUrl(edit.url) : null) : prev.url,
          auth_set: edit && 'auth' in edit ? Boolean(edit.auth) : prev.auth_set,
        };
        cur.health[d.name] ??= { ok: null, consecutive_failures: 0 };
      }
      for (const name of Object.keys(cur.secrets)) {
        if (!upd.settings.destinations.some((d) => d.name === name)) {
          delete cur.secrets[name];
          delete cur.health[name];
        }
      }
      if (upd.smtp_password !== undefined) cur.smtp_password = Boolean(upd.smtp_password);
      commit(db);
    }),
    route('POST', '/api/alerts/test', () => ({ ok: true })),

    // ---- auth
    route('GET', '/auth/me', ({ db }) => db.identity),
    route('POST', '/auth/login', ({ db }) => db.identity),
    route('POST', '/auth/logout', () => undefined),
    route('GET', '/api/keys', ({ db }) => db.apiKeys),
    route('POST', '/api/keys', ({ db, body }) => {
      const key = { id: newId(db, 'key'), name: String(body?.name ?? 'key'), created_at: iso() };
      db.apiKeys.push(key);
      return { ...key, key: `clk_demo_${fakeDigest(key.id).slice(7, 39)}` };
    }),
    route('DELETE', '/api/keys/:id', ({ db, params }) => {
      db.apiKeys = db.apiKeys.filter((k) => k.id !== params.id);
    }),

    // ---- mcp / oauth
    route('GET', '/api/mcp/clients', ({ db }) => db.mcpClients),
    route('DELETE', '/api/mcp/clients/:id', ({ db, params }) => {
      db.mcpClients = db.mcpClients.filter((c) => c.client_id !== params.id);
    }),
    route('GET', '/api/oauth/requests', ({ db }) => db.oauthRequests),
    route('GET', '/api/oauth/requests/:id', ({ db, params }) => {
      const r = need(db.oauthRequests.find((x) => x.id === params.id), 'request');
      return {
        client_name: r.client_name,
        redirect_host: r.redirect_host,
        scope: r.scope,
        requested_grant: r.requested_grant,
        allowed_grants: ['read', 'admin'],
      };
    }),
    route('POST', '/api/oauth/requests/:id/approve', ({ db, params, body }) => {
      const r = need(db.oauthRequests.find((x) => x.id === params.id), 'request');
      db.oauthRequests = db.oauthRequests.filter((x) => x.id !== r.id);
      db.mcpClients.push({
        client_id: newId(db, 'client'),
        client_name: r.client_name,
        grant: body?.grant ?? r.requested_grant,
        approved_by: db.identity.username,
        created_at: nowSecs(),
      });
      return { redirect: '/settings/access' };
    }),
    route('POST', '/api/oauth/requests/:id/deny', ({ db, params }) => {
      db.oauthRequests = db.oauthRequests.filter((x) => x.id !== params.id);
      return { redirect: '/settings/access' };
    }),

    // ---- cluster
    route('GET', '/api/status', ({ db }) => clusterStatus(db)),
    route('GET', '/api/state', ({ db }) => db.state),

    // ---- nodes
    route('GET', '/api/nodes', ({ db }) => db.state.nodes),
    route('GET', '/api/nodes/metrics', ({ db }) => nodeMetrics(db)),
    route('GET', '/api/nodes/connectivity', ({ db }): Connectivity => db.connectivity),
    route('GET', '/api/nodes/:name/metrics/history', ({ db, params }) => metricsHistory(db, params.name)),
    route('GET', '/api/nodes/:name/interfaces', () => ['eth0', 'wlan0', 'tailscale0']),
    route('GET', '/api/nodes/:name', ({ db, params }) => nodeDetail(db, params.name)),
    route('POST', '/api/nodes/join-command', ({ db, body }): JoinCommand => {
      const name = String(body?.name ?? 'new-node');
      const address = String(body?.address ?? '192.0.2.50');
      return {
        command: `clouduler join --name ${name} --address ${address} --key clk_demo_${fakeDigest(name).slice(7, 31)} https://${Object.values(db.state.nodes)[0]?.rpc_addr.split(':')[0] ?? '192.0.2.1'}:8443`,
        key_id: newId(db, 'key'),
        address,
        fingerprint: db.tlsFingerprint,
      };
    }),
    route('POST', '/api/nodes', ({ db, body }) => {
      const name = String(body?.name ?? '');
      if (!name) throw new HttpError(400, 'name is required');
      db.state.nodes[name] = {
        id: newId(db, 'node'),
        name,
        role: body?.role ?? 'worker',
        rpc_addr: `${body?.address ?? '192.0.2.50'}:7373`,
        drives: {},
        availability: 'active',
        labels: {},
        egress_networks: [],
        networks: [],
        open_ports: {},
        arch: 'x86_64',
        binary_hash: '',
        node_pubkey: null,
        memory_total_bytes: 0,
        last_resort: false,
        version: '',
      };
      commit(db);
    }),
    route('PATCH', '/api/nodes/:name', ({ db, params, body }) => {
      need(db.state.nodes[params.name], `node ${params.name}`).availability = body?.availability === 'drain' ? 'drain' : 'active';
      commit(db);
    }),
    route('DELETE', '/api/nodes/:name', ({ db, params }) => {
      need(db.state.nodes[params.name], `node ${params.name}`);
      delete db.state.nodes[params.name];
      delete db.config.nodes[params.name];
      delete db.usage[params.name];
      db.state.removed.push(params.name);
      db.state.unreachable = db.state.unreachable.filter((n) => n !== params.name);
      commit(db);
    }),

    // ---- stacks
    route('GET', '/api/stacks', ({ db }) =>
      Object.values(db.state.stacks).map((s) => ({ name: s.name, revision: s.revision, deployed_at: s.deployed_at })),
    ),
    route('GET', '/api/stacks/:name', ({ db, params }): StackDetailResponse => {
      const s = need(db.state.stacks[params.name], `stack ${params.name}`);
      return { ...s, requirements: requirementsFor(s.spec, domainsOf(db)).requirements };
    }),
    route('PUT', '/api/stacks/:name', ({ db, params, body }) => {
      deployStack(db, params.name, body);
      commit(db);
    }),
    route('DELETE', '/api/stacks/:name', ({ db, params }) => {
      need(db.state.stacks[params.name], `stack ${params.name}`);
      delete db.state.stacks[params.name];
      for (const id of Object.keys(db.state.assignments)) if (id.startsWith(`${params.name}.`)) dropInstance(db, id);
      db.pendingInstances = db.pendingInstances.filter((i) => !i.id.startsWith(`${params.name}.`));
      db.proxyRoutes = db.proxyRoutes.filter((r) => r.stack !== params.name);
      commit(db);
    }),
    route('POST', '/api/stacks/:name/validate', ({ db, body }) => validateStack(db, body?.spec)),
    route('GET', '/api/stacks/:name/update-webhook', ({ params }) => ({
      path: `/api/hooks/update/${fakeDigest(params.name).slice(7, 39)}`,
    })),
    route('POST', '/api/stacks/:name/update-webhook/regenerate', ({ params }) => ({
      path: `/api/hooks/update/${fakeDigest(`${params.name}-${Date.now()}`).slice(7, 39)}`,
    })),

    // ---- instances
    route('GET', '/api/instances', ({ db }) => listInstances(db)),
    route('GET', '/api/instances/:id', ({ db, params }) => need(listInstances(db).find((i) => i.id === params.id), `instance ${params.id}`)),
    route('POST', '/api/instances/:id/restart', ({ db, params }) => {
      need(db.state.assignments[params.id], `instance ${params.id}`);
      db.live[params.id] = { ...db.live[params.id], state: 'restarting', running: false, health: 'starting', reason: 'restarting' };
      later(db, 1500, () => {
        db.live[params.id] = {
          ...db.live[params.id],
          state: 'running',
          running: true,
          health: 'healthy',
          reason: null,
          started_at: iso(),
        };
      });
      commit(db);
    }),
    route('GET', '/api/instances/:id/actions', ({ db, params }) => {
      const img = db.live[params.id]?.image ?? '';
      return img.includes('postgres') ? [{ name: 'vacuum', confirm: false }, { name: 'reindex', confirm: true }] : [];
    }),
    route('POST', '/api/instances/:id/actions/:name', ({ params }): ExecRunResult => ({
      duration_ms: 840,
      stdout: `${params.name}: ok\n`,
      stderr: '',
      exit_code: 0,
      timed_out: false,
      stdout_truncated: false,
      stderr_truncated: false,
    })),
    route('POST', '/api/instances/:id/move', ({ db, params, body }) => {
      const a = need(db.state.assignments[params.id], `instance ${params.id}`);
      const to = String(body?.node ?? '');
      need(db.state.nodes[to], `node ${to}`);
      a.migration = { from: a.node, to, phase: 'syncing' } as never;
      const phases = ['verifying', 'switching', 'starting'] as const;
      phases.forEach((p, i) =>
        later(db, 900 * (i + 1), () => {
          if (a.migration) (a.migration as { phase: string }).phase = p;
        }),
      );
      later(db, 900 * 4, () => {
        a.node = to;
        a.migration = null;
      });
      commit(db);
    }),
    route('POST', '/api/instances/:id/migration/retry', ({ db, params }) => {
      const a = need(db.state.assignments[params.id], `instance ${params.id}`);
      if (a.migration) a.migration = { ...a.migration, phase: 'syncing' };
      delete db.live[params.id]?.migration_problem;
      commit(db);
    }),
    route('POST', '/api/instances/:id/migration/dismiss', ({ db, params }) => {
      const a = need(db.state.assignments[params.id], `instance ${params.id}`);
      a.migration = null;
      delete db.live[params.id]?.migration_problem;
      commit(db);
    }),
    route('PATCH', '/api/instances/:id', ({ db, params, body }) => {
      const a = need(db.state.assignments[params.id], `instance ${params.id}`);
      a.desired = body?.desired === 'stopped' ? 'stopped' : 'running';
      db.live[params.id] = {
        ...db.live[params.id],
        state: a.desired === 'stopped' ? 'stopped' : 'running',
        running: a.desired === 'running',
        health: a.desired === 'running' ? 'healthy' : 'none',
      };
      commit(db);
    }),
    route('POST', '/api/instances/:id/update', ({ db, params }) => {
      need(db.state.assignments[params.id], `instance ${params.id}`);
      db.live[params.id] = { ...db.live[params.id], state: 'pulling', reason: 'pulling image' };
      later(db, 2000, () => {
        db.live[params.id] = { ...db.live[params.id], state: 'running', running: true, reason: null };
        const row = db.updates.find((u) => u.id === params.id);
        if (row) {
          row.running_digest = row.latest_digest;
          row.update_available = false;
        }
      });
      commit(db);
    }),
    route('POST', '/api/rebalance', ({ db }) => {
      const moves: { instance: string; from: string; to: string }[] = [];
      db.lastRebalance = { moves, at: nowSecs() };
      return { moves };
    }),
    route('GET', '/api/rebalance/last', ({ db }) => db.lastRebalance),

    // ---- updates
    route('GET', '/api/updates', ({ db }) => db.updates),
    route('POST', '/api/updates/refresh', ({ db }) => {
      for (const u of db.updates) u.checked_at = nowSecs();
      return { started: true };
    }),
    route('POST', '/api/updates/pin', ({ db, body }): BulkResult[] => {
      const ids: string[] = Array.isArray(body?.instances) && body.instances.length ? body.instances : db.updates.map((u) => u.id);
      return ids.map((id) => {
        const row = db.updates.find((u) => u.id === id);
        if (!row) return { id, ok: false, error: 'unknown instance' };
        row.pinned_digest = row.running_digest;
        return { id, ok: true };
      });
    }),
    route('POST', '/api/updates/apply', ({ db, body }) => {
      const ids: string[] = Array.isArray(body?.instances) && body.instances.length
        ? body.instances
        : db.updates.filter((u) => u.update_available).map((u) => u.id);
      for (const id of ids) {
        if (!db.state.assignments[id]) continue;
        db.live[id] = { ...db.live[id], state: 'pulling', reason: 'pulling image' };
        later(db, 2500, () => {
          db.live[id] = { ...db.live[id], state: 'running', running: true, reason: null, started_at: iso() };
          const row = db.updates.find((u) => u.id === id);
          if (row) {
            row.running_digest = row.latest_digest;
            row.pinned_digest = row.latest_digest;
            row.update_available = false;
          }
        });
      }
      commit(db);
      return { accepted: ids };
    }),

    // ---- registries
    route('GET', '/api/registries', ({ db }) => db.registries),
    route('PUT', '/api/registries', ({ db, body }) => {
      db.registries = [...db.registries.filter((r) => r.server !== body?.server), { server: body?.server, username: body?.username }];
    }),
    route('POST', '/api/registries/verify', () => undefined),
    route('DELETE', '/api/registries/:server', ({ db, params }) => {
      db.registries = db.registries.filter((r) => r.server !== params.server);
    }),

    // ---- configs & secrets
    // The server never lists its reserved `_clouduler_*` objects here.
    route('GET', '/api/configs', ({ db }) => Object.values(db.state.configs).filter((c) => !c.name.startsWith('_clouduler_'))),
    route('GET', '/api/configs/:name', ({ db, params }) => need(db.state.configs[params.name], `config ${params.name}`)),
    route('PUT', '/api/configs/:name', ({ db, params, body }) => {
      putConfigEntry(db, params.name, String(body?.content ?? ''));
      commit(db);
    }),
    route('DELETE', '/api/configs/:name', ({ db, params }) => {
      need(db.state.configs[params.name], `config ${params.name}`);
      delete db.state.configs[params.name];
      commit(db);
    }),
    route('GET', '/api/secrets', ({ db }) => secretSummaries(db)),
    route('POST', '/api/secrets/:name/rotate', ({ db, params, body }) => {
      need(db.state.secrets[params.name], `secret ${params.name}`);
      putSecretEntry(db, params.name);
      if (!staleSecrets.has(db)) staleSecrets.set(db, new Set());
      staleSecrets.get(db)!.add(params.name);
      const started = body?.restart
        ? rollSecretUsers(db, params.name)
        : { revision: db.state.secrets[params.name].revision, restarting: [], skipped: [] };
      commit(db);
      return started;
    }),
    route('POST', '/api/secrets/:name/restart', ({ db, params }) => {
      need(db.state.secrets[params.name], `secret ${params.name}`);
      const started = rollSecretUsers(db, params.name);
      commit(db);
      return started;
    }),
    route('PUT', '/api/secrets/:name', ({ db, params, body }) => {
      const req = body as SecretPutRequest;
      if (!req || typeof req !== 'object') throw new HttpError(400, 'body required');
      if (db.state.secrets[params.name]) {
        if (!staleSecrets.has(db)) staleSecrets.set(db, new Set());
        staleSecrets.get(db)!.add(params.name);
      }
      putSecretEntry(db, params.name);
      commit(db);
    }),
    route('DELETE', '/api/secrets/:name', ({ db, params }) => {
      need(db.state.secrets[params.name], `secret ${params.name}`);
      delete db.state.secrets[params.name];
      commit(db);
    }),
    route('GET', '/api/secrets/key', ({ db }) => db.clusterKey),
    route('POST', '/api/secrets/key', ({ db }) => {
      db.clusterKey = { fingerprint: fakeDigest('uploaded-key').slice(7, 23), held: true };
    }),

    // ---- drives & volumes
    route('GET', '/api/drives', ({ db }) => driveRows(db)),
    route('GET', '/api/volumes', ({ db }) => volumeRows(db)),
    route('GET', '/api/volumes/:drive/:volume/inspect', ({ db, params, query }): VolumeInspection => {
      const v = need(db.state.volumes[`${params.drive}/${params.volume}`], `volume ${params.drive}/${params.volume}`);
      return { node: query.get('node') ?? v.primary, files: 18342, bytes: 4_120_000_000, newest_file_at: nowSecs() - 300 };
    }),
    route('POST', '/api/volumes/:drive/:volume/freeze', ({ db, params }) => {
      const v = need(db.state.volumes[`${params.drive}/${params.volume}`], 'volume');
      v.frozen = { node: v.primary, stopped: [] };
      commit(db);
    }),
    route('POST', '/api/volumes/:drive/:volume/unfreeze', ({ db, params }) => {
      need(db.state.volumes[`${params.drive}/${params.volume}`], 'volume').frozen = null;
      commit(db);
    }),
    route('DELETE', '/api/volumes/:drive/:volume', ({ db, params }) => {
      need(db.state.volumes[`${params.drive}/${params.volume}`], 'volume');
      delete db.state.volumes[`${params.drive}/${params.volume}`];
      commit(db);
    }),
    route('POST', '/api/volumes/:drive/:volume/sync', ({ db, params }) => {
      const v = need(db.state.volumes[`${params.drive}/${params.volume}`], 'volume');
      v.generation += 1;
      for (const n of Object.keys(v.replicas)) v.replicas[n] = v.generation;
      commit(db);
    }),

    // ---- configuration
    route('GET', '/api/config', ({ db }) => db.config),
    route('PUT', '/api/config', ({ db, body }) => {
      if (!body?.config || typeof body.config !== 'object') throw new HttpError(400, 'config is required');
      db.config = body.config as ClusterConfig;
      if (body.rollback_after) db.pending = { armed: true, seconds_left: 60 };
      commit(db);
    }),
    route('POST', '/api/config/validate', ({ body }) => {
      const c = body?.config as ClusterConfig | undefined;
      const errors = [];
      if (!c?.cluster?.name) errors.push({ path: 'cluster.name', message: 'a cluster needs a name' });
      return { valid: errors.length === 0, errors };
    }),
    route('POST', '/api/config/confirm', ({ db }) => {
      db.pending = { armed: false, seconds_left: null };
    }),
    route('POST', '/api/config/rollback', ({ db }) => {
      db.pending = { armed: false, seconds_left: null };
    }),
    route('GET', '/api/config/pending', ({ db }) => db.pending),
    route('POST', '/api/config/networks/:name/rename', ({ db, params, body }) => {
      const to = String(body?.new_name ?? '');
      if (!to) throw new HttpError(400, 'new_name is required');
      renameKey(db.config.networks, params.name, to);
      for (const n of Object.values(db.config.nodes)) if (params.name in n.network) renameKey(n.network, params.name, to);
      for (const s of Object.values(db.state.stacks)) s.spec = renameInSpec(s.spec, params.name, to, 'network');
      for (const r of db.proxyRoutes) if (r.network === params.name) r.network = to;
      commit(db);
    }),
    route('POST', '/api/config/drives/:name/rename', ({ db, params, body }) => {
      const to = String(body?.new_name ?? '');
      if (!to) throw new HttpError(400, 'new_name is required');
      renameKey(db.config.drives, params.name, to);
      for (const n of Object.values(db.config.nodes)) if (params.name in n.drives) renameKey(n.drives, params.name, to);
      for (const n of Object.values(db.state.nodes)) if (params.name in n.drives) renameKey(n.drives, params.name, to);
      for (const s of Object.values(db.state.stacks)) s.spec = renameInSpec(s.spec, params.name, to, 'drive');
      for (const [id, v] of Object.entries({ ...db.state.volumes })) {
        if (id.startsWith(`${params.name}/`)) {
          const nid = `${to}/${id.slice(params.name.length + 1)}`;
          delete db.state.volumes[id];
          db.state.volumes[nid] = { ...v, id: nid };
        }
      }
      commit(db);
    }),

    // ---- cloudflare / oidc / proxy auth (write-only secrets)
    route('GET', '/api/cloudflare/token', ({ db }) => ({ configured: db.cloudflareTokenSet })),
    route('PUT', '/api/cloudflare/token', ({ db }) => {
      db.cloudflareTokenSet = true;
    }),
    route('GET', '/api/oidc/client-secret', ({ db }) => ({ configured: db.oidcSecretSet })),
    route('PUT', '/api/oidc/client-secret', ({ db }) => {
      db.oidcSecretSet = true;
    }),
    route('GET', '/api/proxy/remembered-ips', () => []),
    route('DELETE', '/api/proxy/remembered-ips/:id', () => undefined),
    route('GET', '/api/proxy-auth/client-secret', ({ db }) => ({ configured: db.proxyAuthSecretSet })),
    route('PUT', '/api/proxy-auth/client-secret', ({ db }) => {
      db.proxyAuthSecretSet = true;
    }),

    // ---- redirects
    route('GET', '/api/redirects', ({ db }) => db.redirects),
    route('POST', '/api/redirects', ({ db, body }) => {
      const rule = { ...body, id: newId(db, 'redirect') };
      db.redirects.push(rule);
      db.proxyRoutes.push({ host: rule.host, path: rule.path_prefix ?? '/', kind: 'redirect', network: rule.network, tls: true, websockets: false, apex: false });
      commit(db);
      return rule;
    }),
    route('PUT', '/api/redirects/:id', ({ db, params, body }) => {
      const i = db.redirects.findIndex((r) => r.id === params.id);
      if (i < 0) throw new HttpError(404, 'redirect not found');
      db.redirects[i] = { ...body, id: params.id };
      return db.redirects[i];
    }),
    route('DELETE', '/api/redirects/:id', ({ db, params }) => {
      const r = db.redirects.find((x) => x.id === params.id);
      db.redirects = db.redirects.filter((x) => x.id !== params.id);
      if (r) db.proxyRoutes = db.proxyRoutes.filter((p) => !(p.kind === 'redirect' && p.host === r.host));
      commit(db);
    }),

    // ---- static routes
    route('GET', '/api/static-routes', ({ db }) => db.staticRoutes),
    route('POST', '/api/static-routes', ({ db, body }) => {
      const rule = { ...body, id: newId(db, 'static') };
      db.staticRoutes.push(rule);
      db.proxyRoutes.push({ host: rule.host, path: rule.path_prefix ?? '/', kind: 'static', network: rule.network, tls: rule.tls, websockets: rule.websockets, apex: false });
      commit(db);
      return rule;
    }),
    route('PUT', '/api/static-routes/:id', ({ db, params, body }) => {
      const i = db.staticRoutes.findIndex((r) => r.id === params.id);
      if (i < 0) throw new HttpError(404, 'static route not found');
      db.staticRoutes[i] = { ...body, id: params.id };
      return db.staticRoutes[i];
    }),
    route('DELETE', '/api/static-routes/:id', ({ db, params }) => {
      const r = db.staticRoutes.find((x) => x.id === params.id);
      db.staticRoutes = db.staticRoutes.filter((x) => x.id !== params.id);
      if (r) db.proxyRoutes = db.proxyRoutes.filter((p) => !(p.kind === 'static' && p.host === r.host));
      commit(db);
    }),

    // ---- wireguard
    route('PUT', '/api/wireguard/:network/:node', ({ db, params, body }) => {
      if (typeof body?.content !== 'string') throw new HttpError(400, 'content required');
      need(db.config.networks[params.network], `network ${params.network}`);
      putSecretEntry(db, `_clouduler_wg_${params.network}_${params.node}`);
      commit(db);
      return { interface: `wg-${params.network}`.slice(0, 15) };
    }),
    route('DELETE', '/api/wireguard/:network/:node', ({ db, params }) => {
      delete db.state.secrets[`_clouduler_wg_${params.network}_${params.node}`];
      commit(db);
    }),
    route('GET', '/api/wireguard-ingress', ({ db }) => db.wireguardIngress),
    route('POST', '/api/wireguard-ingress', ({ db, body }) => {
      const rule = { ...body, id: newId(db, 'wgin') };
      db.wireguardIngress.push(rule);
      return rule;
    }),
    route('PUT', '/api/wireguard-ingress/:id', ({ db, params, body }) => {
      const i = db.wireguardIngress.findIndex((r) => r.id === params.id);
      if (i < 0) throw new HttpError(404, 'rule not found');
      db.wireguardIngress[i] = { ...body, id: params.id };
      return db.wireguardIngress[i];
    }),
    route('DELETE', '/api/wireguard-ingress/:id', ({ db, params }) => {
      db.wireguardIngress = db.wireguardIngress.filter((r) => r.id !== params.id);
    }),

    // ---- certificates & dns
    route('GET', '/api/certificates', ({ db }) => certRows(db)),
    route('POST', '/api/certificates/:fqdn/renew', ({ db, params }) => {
      const c = need(db.state.certificates[params.fqdn], `certificate ${params.fqdn}`);
      c.issued_at = nowSecs();
      c.not_after = nowSecs() + 86400 * 90;
      commit(db);
    }),
    route('GET', '/api/tls-fingerprint', ({ db }) => ({ fingerprint: db.tlsFingerprint })),
    route('GET', '/api/dns', ({ db }) => dnsRows(db)),
    route('GET', '/api/proxy/routes', ({ db }) => db.proxyRoutes),
    route('POST', '/api/dns/reconcile', ({ db }) => {
      db.state.dns_status = {};
      commit(db);
    }),

    // ---- self-update
    route('GET', '/api/self-update/github-token', ({ db }) => ({ configured: db.selfUpdate.githubTokenSet })),
    route('PUT', '/api/self-update/github-token', ({ db }) => {
      db.selfUpdate.githubTokenSet = true;
    }),
    route('PUT', '/api/self-update/repo', ({ db, body }) => {
      db.selfUpdate.repo = body;
    }),
    route('PUT', '/api/self-update/check', ({ db, body }) => {
      db.selfUpdate.repo = {
        release_url: db.selfUpdate.repo?.release_url ?? 'https://github.com/example/clouduler',
        check_updates: Boolean(body?.enabled),
        check_interval: body?.interval ?? '24h',
      };
    }),
    route('GET', '/api/self-update/latest', ({ db }) => db.selfUpdate.latest),
    route('GET', '/api/self-update/repo', ({ db }) => db.selfUpdate.repo),
    route('POST', '/api/self-update/run', ({ db }) => {
      const nodes = Object.keys(db.state.nodes);
      db.selfUpdate.run = {
        id: newId(db, 'upd'),
        release_tag: db.selfUpdate.latest.tag,
        owner: 'example',
        repo: 'clouduler',
        assets: {},
        arch_hashes: {},
        per_node: Object.fromEntries(nodes.map((n) => [n, 'fetching' as const])),
        started_at: nowSecs(),
        finished_at: null,
        failure_reason: null,
        restart_started_at: null,
      };
      putReservedConfig(db, '_clouduler_self_update_run', db.selfUpdate.run);
      commit(db);
      return db.selfUpdate.run;
    }),
    route('GET', '/api/self-update/run', ({ db }) => db.selfUpdate.run),
    route('POST', '/api/self-update/cancel', ({ db }) => {
      db.selfUpdate.run = null;
      delete db.state.configs['_clouduler_self_update_run'];
      commit(db);
    }),

    // ---- git backup
    route('GET', '/api/git-backup/status', ({ db }) => db.gitBackup),
    route('POST', '/api/git-backup/export-now', ({ db }) => {
      db.gitBackup.last_export = { at: nowSecs(), summary: 'exported 12 objects', commit: fakeDigest('export').slice(7, 14), error: null };
      putReservedConfig(db, '_clouduler_git_backup_last_export', db.gitBackup.last_export);
      commit(db);
    }),
    route('POST', '/api/git-backup/pull-now', ({ db }) => {
      db.gitBackup.last_pull = { at: nowSecs(), commit: null, rejected: null };
      putReservedConfig(db, '_clouduler_git_backup_last_pull', db.gitBackup.last_pull);
      commit(db);
    }),
    route('POST', '/api/git-backup/reconcile-now', () => undefined),
    route('GET', '/api/git-backup/webhook-secret', ({ db }) => ({ configured: db.gitWebhookSecretSet })),
    route('PUT', '/api/git-backup/webhook-secret', ({ db }) => {
      db.gitWebhookSecretSet = true;
    }),

    // ---- restic backups
    route('GET', '/api/restic/destinations', ({ db }) => db.backupDestinations),
    route('PUT', '/api/restic/destinations', ({ db, body }) => {
      if (!body?.id) throw new HttpError(400, 'id required');
      const kind = body.kind as BackupDestinationKind;
      const dest: BackupDestination = { id: body.id, name: body.name, kind, password_secret: `_clouduler_restic_${body.id}` };
      db.state.restic_destinations[body.id] = dest;
      const summary = { id: body.id, name: body.name, kind, has_credentials: Boolean(body.ssh_private_key || body.s3_secret_access_key) };
      db.backupDestinations = [...db.backupDestinations.filter((d) => d.id !== body.id), summary];
      commit(db);
    }),
    route('DELETE', '/api/restic/destinations/:id', ({ db, params }) => {
      delete db.state.restic_destinations[params.id];
      db.backupDestinations = db.backupDestinations.filter((d) => d.id !== params.id);
      commit(db);
    }),
    route('PUT', '/api/restic/targets', ({ db, body }) => {
      if (!body?.id) throw new HttpError(400, 'id required');
      db.state.restic_targets[body.id] = { ...db.state.restic_targets[body.id], ...body };
      commit(db);
    }),
    route('DELETE', '/api/restic/targets/:id', ({ db, params }) => {
      delete db.state.restic_targets[params.id];
      commit(db);
    }),
    route('POST', '/api/restic/targets/:id/run', ({ db, params }) => {
      const t = need(db.state.restic_targets[params.id], `target ${params.id}`);
      const run = sampleRun(db, t.id, t.drive, t.destination_id);
      db.state.restic_runs[run.id] = run;
      db.backupRuns.push(run);
      t.last_run = run.started_at;
      commit(db);
      return run.id;
    }),
    route('GET', '/api/restic/runs', ({ db, query }) => {
      const target = query.get('target');
      return db.backupRuns.filter((r) => !target || r.target_id === target);
    }),
    route('GET', '/api/restic/destinations/:id/snapshots', ({ db }) => db.snapshots),
    route('POST', '/api/restic/restore/drive', ({ db, body }) => restore(db, { Drive: { drive: body?.drive } }, body)),
    route('POST', '/api/restic/restore/volume', ({ db, body }) => restore(db, { Volume: { volume: `${body?.drive}/${body?.volume}` } }, body)),
    route('POST', '/api/restic/restore/service', ({ db, body }) => restore(db, { Service: { stack: body?.stack, service: body?.service } }, body)),
    route('GET', '/api/restic/restore-requests', ({ db }) => db.restoreRequests),
  ];
}

function restore(db: MockDb, scope: import('../api/types').RestoreScope, body: { destination_id?: string | null; snapshot?: string | null }) {
  const req = {
    id: newId(db, 'restore'),
    scope,
    destination_id: body?.destination_id ?? null,
    snapshot: body?.snapshot ?? null,
    requested_at: nowSecs(),
    finished_at: null as number | null,
    status: 'Pending' as const,
    requested_by: db.identity.username,
  };
  db.restoreRequests.push(req);
  db.state.restore_requests[req.id] = req;
  later(db, 3000, () => {
    req.status = 'Succeeded' as never;
    req.finished_at = nowSecs();
  });
  commit(db);
  return req.id;
}

// --------------------------------------------------------------- streams ---

const LOG_TEMPLATES = [
  'listening on 0.0.0.0:{port}',
  'GET /healthz 200 1ms',
  'GET / 200 12ms',
  'connection accepted from 10.0.0.{n}',
  'background job finished in {n}ms',
  'cache hit ratio 0.{n}',
  'POST /api/sync 204 8ms',
  'WARN slow query took {n}00ms',
];

function logLine(i: number, id: string): { text: string; timestamp: string } {
  const t = LOG_TEMPLATES[(i * 7 + id.length) % LOG_TEMPLATES.length]
    .replace('{port}', '8080')
    .replace(/\{n\}/g, String(10 + ((i * 13) % 80)));
  return { text: t, timestamp: new Date().toISOString() };
}

export function buildStreamRoutes(): StreamRoute[] {
  return [
    {
      pattern: '/api/events',
      start: ({ db, emit }) => {
        const send = () => emit('state', db.state);
        send();
        db.listeners.add(send);
        return () => db.listeners.delete(send);
      },
    },
    {
      pattern: '/api/instances/events',
      start: ({ db, emit }) => {
        const last = new Map<string, string>();
        const send = () => {
          for (const i of listInstances(db)) {
            const key = `${i.state}|${i.reason ?? ''}|${i.step?.join('/') ?? ''}`;
            if (last.get(i.id) === key) continue;
            last.set(i.id, key);
            const ev: InstanceProgressEvent = { instance_id: i.id, state: i.state, reason: i.reason ?? null, step: i.step ?? null };
            emit('progress', ev);
          }
        };
        send();
        db.listeners.add(send);
        return () => db.listeners.delete(send);
      },
    },
    {
      pattern: '/api/instances/:id/logs',
      start: ({ params, query, emit }) => {
        const tail = Math.min(Number(query.get('tail') ?? 50) || 50, 60);
        const base = Date.now() - tail * 4000;
        for (let i = 0; i < tail; i++) {
          const l = logLine(i, params.id);
          emit(l.text.startsWith('WARN') ? 'stderr' : 'stdout', { ...l, timestamp: new Date(base + i * 4000).toISOString() } satisfies Pick<LogLine, 'timestamp'> & { text: string });
        }
        if (query.get('follow') === 'false') return () => undefined;
        let n = tail;
        const timer = setInterval(() => {
          const l = logLine(n++, params.id);
          emit(l.text.startsWith('WARN') ? 'stderr' : 'stdout', l);
        }, 2500);
        return () => clearInterval(timer);
      },
    },
    {
      pattern: '/api/instances/:id/stats',
      start: ({ params, emit }) => {
        let rx = 1_000_000;
        let tx = 400_000;
        let rd = 0;
        let wr = 0;
        let i = 0;
        const send = () => {
          i += 1;
          rx += 20_000 + (i % 7) * 3000;
          tx += 9_000 + (i % 5) * 2000;
          rd += 4096 * (i % 3);
          wr += 16384 * (1 + (i % 4));
          const s: InstanceStatsSample = {
            cpu_percent: Math.max(0.2, 4 + Math.sin(i / 4 + params.id.length) * 3 + (i % 3)),
            memory_used_bytes: 180_000_000 + Math.round(Math.sin(i / 9) * 8_000_000),
            memory_limit_bytes: 512 * 1024 * 1024,
            network_rx_bytes: rx,
            network_tx_bytes: tx,
            io_read_bytes: rd,
            io_write_bytes: wr,
          };
          emit('stats', s);
        };
        send();
        const timer = setInterval(send, 2000);
        return () => clearInterval(timer);
      },
    },
    {
      pattern: '/api/nodes/:name/logs',
      start: ({ params, query, emit }) => {
        const tail = Math.min(Number(query.get('tail') ?? 100) || 100, 80);
        const msgs: [NodeLogLine['level'], string, string][] = [
          ['info', 'cl_raft', 'heartbeat acknowledged by all voters'],
          ['info', 'cl_sched', 'reconcile pass finished: 0 changes'],
          ['debug', 'cl_drive', 'volume manifests match, nothing to sync'],
          ['warn', 'cl_proxy', 'slow upstream response (410ms)'],
          ['info', 'cl_dns', 'records already match, nothing to publish'],
        ];
        let seq = 0;
        const line = (): NodeLogLine => {
          const [level, target, message] = msgs[seq % msgs.length];
          seq += 1;
          return { seq, timestamp: Date.now() - (tail - seq) * 3000, level, target, message: `${message} (${params.name})` };
        };
        for (let i = 0; i < tail; i++) emit('log', line());
        if (query.get('follow') === 'false') return () => undefined;
        const timer = setInterval(() => emit('log', { ...line(), timestamp: Date.now() }), 3000);
        return () => clearInterval(timer);
      },
    },
  ];
}
