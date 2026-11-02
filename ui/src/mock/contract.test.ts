// Holds the mock backend to the same contract as the real Rust API.
//
// api-shapes.json is generated from real responses by a cl-api test, which
// fails when the server's output drifts from it. Here every endpoint it
// covers is requested from the mock, in every scenario, and the response must
// have the fixture's shape (shape.ts has the exact rules) and be assignable to
// the interface the UI types it with. So a server-side field rename breaks the
// fixture, which breaks this test, which points at the mock to update.

import { describe, expect, it } from 'vitest';
import shapes from '../api/__fixtures__/api-shapes.json';
import type {
  AlertCatalogEntry,
  AlertSettingsResponse,
  AlertsResponse,
  CertificateState,
  CloudflareTokenStatus,
  ClusterConfig,
  ClusterKeyStatus,
  ClusterState,
  ClusterStatus,
  Connectivity,
  DnsRecordState,
  DriveStatus,
  Identity,
  InstanceState,
  LastRebalance,
  NodeDetail,
  NodeMetrics,
  OidcClientSecretStatus,
  PendingConfig,
  ProxyRoute,
  SelfUpdateConfig,
  SelfUpdateGithubTokenStatus,
  SelfUpdateRunState,
  StackDetailResponse,
  StackState,
  UpdateRow,
  VolumeState,
} from '../api/types';
import { getTransport, setTransport } from '../api/transport';
import { buildScenario, scenarios } from './scenarios';
import { createMockTransport } from './transport';
import { shapeDiffs, type ShapeOptions } from './shape';

/** GET a path from the mock and parse it, failing loudly on a non-200. */
async function getJson<T>(path: string): Promise<T> {
  const res = await getTransport().fetch(path, { method: 'GET' });
  expect(res.status, `${path} answered ${res.status}`).toBe(200);
  const text = await res.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

/** The first `state` event of /api/events, as the app's store receives it. */
function firstStateEvent(): Promise<ClusterState> {
  return new Promise((resolve, reject) => {
    const es = getTransport().eventSource('/api/events');
    const timer = setTimeout(() => reject(new Error('no state event from /api/events')), 2000);
    es.addEventListener('state', (e) => {
      clearTimeout(timer);
      es.close();
      resolve(JSON.parse(e.data) as ClusterState);
    });
  });
}

// Paths use `.key`, `[]` (array item) and `.*` (map value); see shape.ts.
//
// `optional` lists keys the Rust side only serialises when set (serde
// skip_serializing_if), which the fixture harness - one container-less node,
// no registry, no stored settings - never had a value for.
const INSTANCE_OPTIONAL = [
  '[].running_digest', '[].exit_code', '[].oom_killed', '[].restart_limit', '[].finished_at',
  '[].memory_limit_bytes', '[].proxy', '[].reason', '[].step', '[].placement_error', '[].migration_problem',
];
const ALERT_VIEW_OPTIONAL = [
  'escalated_from', 'instance', 'node', 'stack', 'resolved_at', 'excerpt', 'deliveries',
].map((k) => `[].${k}`);

const OPTIONS: Record<string, ShapeOptions> = {
  alertCatalog: {},
  alertSettings: {
    maps: ['.health', '.secrets'],
    optional: [
      '.health.*.error', '.health.*.at', '.settings.timezone', '.settings.public_url', '.settings.smtp',
      '.secrets.*.url', '.settings.destinations[].categories', '.settings.destinations[].stacks',
      // the fixture harness only had a webhook destination; these belong to the discord and email kinds
      '.settings.destinations[].style', '.settings.destinations[].notify', '.settings.destinations[].buttons', '.settings.destinations[].to',
    ],
    any: ['.settings.events'],
  },
  alerts: {
    optional: [
      ...ALERT_VIEW_OPTIONAL.map((p) => p.replace('[]', '.active[]')),
      ...ALERT_VIEW_OPTIONAL.map((p) => p.replace('[]', '.recent[]')),
      '.active[].deliveries[].error', '.active[].deliveries[].message_id',
    ],
  },
  certificates: { optional: ['[].error', '[].error_at'] },
  cloudflareTokenStatus: {},
  clusterConfig: {
    maps: ['.networks', '.drives', '.nodes', '.nodes.*.network', '.nodes.*.drives'],
    optional: [
      '.cloudflare', '.self_update', '.backup', '.proxy.acme.contact_email', '.proxy.acme.directory_url',
      '.proxy.auth', '.auth.oidc', '.networks.*.subnet', '.networks.*.domain', '.networks.*.publish_dns',
      '.drives.*.served_by', '.nodes.*.data_dir', '.nodes.*.docker_data_dir', '.nodes.*.last_resort',
      '.nodes.*.generate_secret_key',
    ],
    // a node address is a bare string or a tagged object
    any: ['.nodes.*.network.*'],
  },
  clusterKeyStatus: {},
  dns: {},
  drives: { optional: ['[].active_server'] }, // null for a replicated drive; the fixture only has a served one
  identity: {},
  instances: { optional: INSTANCE_OPTIONAL },
  lastRebalance: {},
  nodeConnectivity: {},
  nodeDetail: { optional: ['.usage.version'] },
  nodeMetrics: {},
  oidcClientSecretStatus: {},
  pendingConfig: {},
  proxyRoutes: {},
  selfUpdateGithubToken: {},
  selfUpdateRepo: {},
  selfUpdateRun: {},
  stackDetail: { maps: ['.requirements.allowed'], optional: ['.deployed_at', '.previous_spec'] },
  stacks: { optional: ['[].deployed_at'] },
  state: {
    maps: [
      '.nodes', '.nodes.*.drives', '.nodes.*.labels', '.nodes.*.open_ports',
      '.stacks', '.assignments', '.volumes', '.volumes.*.replicas', '.configs', '.secrets',
      '.certificates', '.dns_records', '.cert_status', '.dns_status', '.wrapped_keys',
      '.alert_destinations', '.alerts', '.alert_inbox', '.container_status', '.crash_moves',
      '.previous_image_digests', '.volume_removals', '.instance_updates',
      '.restic_destinations', '.restic_targets', '.restic_runs', '.restore_requests',
      '.sessions', '.proxy_sessions', '.proxy_tokens', '.stacks.*.requirements.allowed',
    ],
    optional: [
      '.nodes.*.version', '.stacks.*.deployed_at', '.stacks.*.previous_spec', '.cluster_config',
    ],
    // enum-shaped values: the fixture only shows one arm each
    any: ['.assignments.*.migration', '.cert_status.*', '.dns_status.*', '.alerts.*', '.alert_destinations.*'],
  },
  status: {},
  updates: { optional: ['[].error'] },
  volumes: {},
};

interface Endpoint {
  fixture: keyof typeof shapes;
  path: (db: ReturnType<typeof buildScenario>) => string;
}

const ENDPOINTS: Endpoint[] = [
  { fixture: 'alertCatalog', path: () => '/api/alerts/catalog' },
  { fixture: 'alertSettings', path: () => '/api/alerts/settings' },
  { fixture: 'alerts', path: () => '/api/alerts' },
  { fixture: 'certificates', path: () => '/api/certificates' },
  { fixture: 'cloudflareTokenStatus', path: () => '/api/cloudflare/token' },
  { fixture: 'clusterConfig', path: () => '/api/config' },
  { fixture: 'clusterKeyStatus', path: () => '/api/secrets/key' },
  { fixture: 'dns', path: () => '/api/dns' },
  { fixture: 'drives', path: () => '/api/drives' },
  { fixture: 'identity', path: () => '/auth/me' },
  { fixture: 'instances', path: () => '/api/instances' },
  { fixture: 'lastRebalance', path: () => '/api/rebalance/last' },
  { fixture: 'nodeConnectivity', path: () => '/api/nodes/connectivity' },
  { fixture: 'nodeDetail', path: (db) => `/api/nodes/${Object.keys(db.state.nodes)[0]}` },
  { fixture: 'nodeMetrics', path: () => '/api/nodes/metrics' },
  { fixture: 'oidcClientSecretStatus', path: () => '/api/oidc/client-secret' },
  { fixture: 'pendingConfig', path: () => '/api/config/pending' },
  { fixture: 'proxyRoutes', path: () => '/api/proxy/routes' },
  { fixture: 'selfUpdateGithubToken', path: () => '/api/self-update/github-token' },
  { fixture: 'selfUpdateRepo', path: () => '/api/self-update/repo' },
  { fixture: 'selfUpdateRun', path: () => '/api/self-update/run' },
  {
    fixture: 'stackDetail',
    // fresh-single-node has no stacks; the request below is skipped for it.
    path: (db) => `/api/stacks/${Object.keys(db.state.stacks)[0]}`,
  },
  { fixture: 'stacks', path: () => '/api/stacks' },
  { fixture: 'status', path: () => '/api/status' },
  { fixture: 'updates', path: () => '/api/updates' },
  { fixture: 'volumes', path: () => '/api/volumes' },
];

/** Every key of the fixture is either exercised above or deliberately listed here. */
const COVERED_ELSEWHERE = ['state'];

describe('mock fixtures cover the same endpoints as the real-api fixture', () => {
  it('has a mock request for every key of api-shapes.json', () => {
    const have = new Set<string>([...ENDPOINTS.map((e) => e.fixture), ...COVERED_ELSEWHERE]);
    const missing = Object.keys(shapes).filter((k) => !have.has(k));
    expect(missing, 'add these endpoints to ENDPOINTS in mock/contract.test.ts').toEqual([]);
  });
});

for (const name of Object.keys(scenarios)) {
  describe(`mock contract: scenario ${name}`, () => {
    const db = buildScenario(name);
    setTransport(createMockTransport(db));

    // Compile-time half: the scenario's own data is assigned to the
    // interfaces the ui types it with. If a UI interface stops matching what
    // the mock serves, tsc fails here.
    it('serves data assignable to the ui interfaces', async () => {
      setTransport(createMockTransport(db));
      const state: ClusterState = db.state;
      const stacks: StackState[] = Object.values(db.state.stacks);
      const alerts: AlertsResponse = db.alerts;
      const catalog: AlertCatalogEntry[] = db.alertCatalog;
      const settings: AlertSettingsResponse = db.alertSettings;
      const config: ClusterConfig = db.config;
      const pending: PendingConfig = db.pending;
      const connectivity: Connectivity = db.connectivity;
      const rows: UpdateRow[] = db.updates;
      const routes: ProxyRoute[] = db.proxyRoutes;
      const identity: Identity = db.identity;
      const rebalance: LastRebalance | null = db.lastRebalance;
      const key: ClusterKeyStatus = db.clusterKey;
      const repo: SelfUpdateConfig | null = db.selfUpdate.repo;
      const run: SelfUpdateRunState | null = db.selfUpdate.run;
      expect([state, stacks, alerts, catalog, settings, config, pending, connectivity, rows, routes, identity, rebalance, key, repo, run]).toBeDefined();

      // And the same, through the wire, for what the routes derive.
      const wire = {
        status: await getJson<ClusterStatus>('/api/status'),
        instances: await getJson<InstanceState[]>('/api/instances'),
        volumes: await getJson<VolumeState[]>('/api/volumes'),
        certificates: await getJson<CertificateState[]>('/api/certificates'),
        dns: await getJson<DnsRecordState[]>('/api/dns'),
        drives: await getJson<DriveStatus[]>('/api/drives'),
        nodeMetrics: await getJson<NodeMetrics>('/api/nodes/metrics'),
        cloudflare: await getJson<CloudflareTokenStatus>('/api/cloudflare/token'),
        oidc: await getJson<OidcClientSecretStatus>('/api/oidc/client-secret'),
        github: await getJson<SelfUpdateGithubTokenStatus>('/api/self-update/github-token'),
      };
      expect(wire.status.node).toBeTruthy();
      const firstNode = Object.keys(db.state.nodes)[0];
      const detail = await getJson<NodeDetail>(`/api/nodes/${firstNode}`);
      expect(detail.name).toBe(firstNode);
      const firstStack = Object.keys(db.state.stacks)[0];
      if (firstStack) {
        const sd = await getJson<StackDetailResponse>(`/api/stacks/${firstStack}`);
        expect(sd.requirements).toBeDefined();
      }
    });

    for (const { fixture, path } of ENDPOINTS) {
      it(`${fixture} (${path(db)}) has the shape the rust api sends`, async () => {
        setTransport(createMockTransport(db));
        if (fixture === 'stackDetail' && Object.keys(db.state.stacks).length === 0) return;
        const body = await getJson<unknown>(path(db));
        const diffs = shapeDiffs(body, shapes[fixture], OPTIONS[fixture]);
        expect(diffs, `${fixture} drifted from the real api:\n${diffs.join('\n')}`).toEqual([]);
      });
    }

    it('state (GET /api/state and the /api/events stream) has the shape the rust api sends', async () => {
      setTransport(createMockTransport(db));
      const viaGet = await getJson<unknown>('/api/state');
      const viaEvent = await firstStateEvent();
      for (const body of [viaGet, viaEvent]) {
        const diffs = shapeDiffs(body, shapes.state, OPTIONS.state);
        expect(diffs, `state drifted from the real api:\n${diffs.join('\n')}`).toEqual([]);
      }
      expect(viaEvent.version).toBe(viaGet && (viaGet as ClusterState).version);
    });
  });
}

describe('the shape checker itself', () => {
  it('flags a missing key, an extra key and a wrong type, in both directions', () => {
    const fixture = { a: 1, b: { c: 'x' }, list: [{ k: 1 }] };
    expect(shapeDiffs({ a: 1, b: { c: 'y' }, list: [{ k: 2 }] }, fixture)).toEqual([]);
    expect(shapeDiffs({ a: 1, b: {}, list: [] }, fixture).join()).toMatch(/\.b\.c: missing/);
    expect(shapeDiffs({ a: 1, b: { c: 'x', d: 1 }, list: [] }, fixture).join()).toMatch(/\.b\.d: the server never sends/);
    expect(shapeDiffs({ a: '1', b: { c: 'x' }, list: [] }, fixture).join()).toMatch(/\.a: is string/);
    expect(shapeDiffs({ a: null, b: { c: 'x' }, list: [] }, fixture).join()).toMatch(/\.a: null/);
  });
});
