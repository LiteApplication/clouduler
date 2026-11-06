// Typed helpers for assembling a scenario. A scenario file is a short script:
//
//   const b = new ScenarioBuilder({ name: 'lan-3-nodes', cluster: 'homelab', ... });
//   b.network('lan', { type: 'private', subnet: '192.168.1.0/24', domain: 'home.example.com' });
//   b.node({ name: 'atlas', ip: '192.168.1.10', role: 'manager' });
//   b.stack('vaultwarden', SPEC, { placement: { app: 'atlas' } });
//   export default b.build();
//
// Everything derived from those calls (instances, volumes, proxy routes,
// certificates, DNS records, update rows, connectivity) is generated here so a
// scenario only states what is interesting about it.

import { encodeText } from '../api/client';
import type {
  AlertCatalogEntry,
  AlertSettingsResponse,
  AlertView,
  Assignment,
  CertificateState,
  ClusterConfig,
  DnsRecordState,
  InstanceState,
  Migration,
  NetworkConfig,
  NodeAddress,
  ProxyRoute,
  Role,
  StackState,
  UpdateRow,
  VolumeState,
} from '../api/types';
import { requirementsFor, parseSpec } from './spec';
import type { MockClusterState, MockDb, MockNodeState } from './db';

export interface ScenarioInfo {
  name: string;
  description: string;
  cluster: string;
}

export interface NodeInput {
  name: string;
  /** Address on the node's primary network. */
  ip: string;
  /** Network the address belongs to; defaults to the first declared network. */
  network?: string;
  role?: Role;
  /** drive name -> backing path; defaults to every declared drive. */
  drives?: Record<string, string>;
  cpus?: number;
  memGiB?: number;
  version?: string;
  labels?: Record<string, string>;
  availability?: 'active' | 'drain';
  /** Unreachable: no metrics, containers unknown, listed in `unreachable`. */
  down?: boolean;
  cpuPercent?: number;
  memUsedPercent?: number;
  /** Extra network -> address entries beyond the primary one. */
  extraNetworks?: Record<string, NodeAddress>;
  site?: string;
}

export interface StackOptions {
  /** service -> node name (or one per replica). Unlisted services spread round-robin. */
  placement?: Record<string, string | string[]>;
  /** Overrides for individual instances by `stack.service.index`. */
  live?: Record<string, Partial<InstanceState>>;
  /** Services whose registry has a newer image. */
  updateAvailable?: string[];
  secrets?: string[];
  configs?: Record<string, string>;
  deployedAgoSecs?: number;
}

const HEX = '0123456789abcdef';
export function fakeDigest(seed: string): string {
  let h = 2166136261;
  let out = '';
  for (let i = 0; i < 64; i++) {
    h = Math.imul(h ^ (seed.charCodeAt(i % seed.length) + i), 16777619) >>> 0;
    out += HEX[h % 16];
  }
  return `sha256:${out}`;
}

export const GIB = 1024 ** 3;

export class ScenarioBuilder {
  private info: ScenarioInfo;
  private now = Math.floor(Date.now() / 1000);
  private networks: ClusterConfig['networks'] = {};
  private drives: ClusterConfig['drives'] = {};
  private nodeInputs: NodeInput[] = [];
  private stacks: { name: string; spec: string; opts: StackOptions }[] = [];
  private extraRoutes: ProxyRoute[] = [];
  private alertViews: AlertView[] = [];
  private tweaks: ((db: MockDb) => void)[] = [];

  constructor(info: ScenarioInfo) {
    this.info = info;
  }

  network(name: string, cfg: { type: NetworkConfig['type']; subnet?: string; domain?: string }) {
    this.networks[name] = cfg as NetworkConfig;
    return this;
  }

  drive(name: string, cfg: { mode?: 'replicated' | 'served'; servedBy?: string[]; syncInterval?: string } = {}) {
    this.drives[name] = {
      mode: cfg.mode ?? 'replicated',
      served_by: cfg.servedBy ?? [],
      sync_interval: cfg.syncInterval ?? '15m',
    };
    return this;
  }

  node(input: NodeInput) {
    this.nodeInputs.push(input);
    return this;
  }

  stack(name: string, spec: string, opts: StackOptions = {}) {
    this.stacks.push({ name, spec, opts });
    return this;
  }

  alert(view: Omit<AlertView, 'id'> & { id?: string }) {
    this.alertViews.push({ id: view.id ?? `alert-${this.alertViews.length + 1}`, ...view });
    return this;
  }

  /** Escape hatch for anything a scenario wants to adjust after assembly. */
  tweak(fn: (db: MockDb) => void) {
    this.tweaks.push(fn);
    return this;
  }

  build(): MockDb {
    const now = this.now;
    const netNames = Object.keys(this.networks);
    const domains = Object.fromEntries(netNames.map((n) => [n, this.networks[n].domain ?? 'example.com']));
    const driveNames = Object.keys(this.drives);

    // ---- nodes
    const nodes: Record<string, MockNodeState> = {};
    const nodeCfg: Record<string, unknown> = {};
    const usage: MockDb['usage'] = {};
    const down: string[] = [];
    this.nodeInputs.forEach((n, i) => {
      const net = n.network ?? netNames[0];
      const drives = n.drives ?? Object.fromEntries(driveNames.map((d) => [d, `/mnt/${d}`]));
      nodes[n.name] = {
        id: `node-${n.name}`,
        name: n.name,
        role: n.role ?? 'manager',
        rpc_addr: `${n.ip}:7373`,
        drives,
        availability: n.availability ?? 'active',
        labels: n.labels ?? {},
        egress_networks: [],
        networks: [net, ...Object.keys(n.extraNetworks ?? {})],
        open_ports: {},
        arch: 'x86_64',
        binary_hash: fakeDigest(`bin-${n.name}`).slice(7, 39),
        node_pubkey: `pk-${n.name}`,
        memory_total_bytes: (n.memGiB ?? 16) * GIB,
        last_resort: false,
        version: n.version ?? '0.9.10',
      };
      nodeCfg[n.name] = {
        id: `node-${n.name}`,
        role: n.role ?? 'manager',
        network: { [net]: n.ip, ...(n.extraNetworks ?? {}) },
        drives,
        rpc_port: 7373,
      };
      if (n.down) down.push(n.name);
      else {
        const total = (n.memGiB ?? 16) * GIB;
        usage[n.name] = {
          cpu_count: n.cpus ?? 8,
          cpu_percent: n.cpuPercent ?? 8 + ((i * 7) % 20),
          load_1m: 0.4 + i * 0.2,
          local: true,
          memory_total_bytes: total,
          memory_used_bytes: Math.round(total * ((n.memUsedPercent ?? 38 + i * 6) / 100)),
          uptime_seconds: 86400 * (12 + i * 3) + 3600,
          version: n.version ?? '0.9.10',
          drives: Object.entries(drives).map(([drive, path]) => ({
            drive,
            path,
            total_bytes: 1000 * GIB,
            free_bytes: (620 - i * 80) * GIB,
          })),
        };
      }
    });
    const upNodes = this.nodeInputs.filter((n) => !n.down && (n.availability ?? 'active') === 'active').map((n) => n.name);

    // ---- stacks, instances, volumes
    const stacks: Record<string, StackState> = {};
    const assignments: Record<string, Assignment> = {};
    const volumes: Record<string, VolumeState> = {};
    const live: MockDb['live'] = {};
    const updates: UpdateRow[] = [];
    const proxyRoutes: ProxyRoute[] = [...this.extraRoutes];
    const certificates: Record<string, CertificateState> = {};
    const dnsRecords: MockClusterState['dns_records'] = {};
    const configs: MockClusterState['configs'] = {};
    const secrets: MockClusterState['secrets'] = {};
    let rr = 0;

    for (const { name, spec, opts } of this.stacks) {
      const { parsed, requirements } = requirementsFor(spec, domains);
      stacks[name] = {
        name,
        spec,
        revision: 1,
        configs: Object.keys(opts.configs ?? {}),
        secrets: opts.secrets ?? [],
        used_configs: requirements.configs,
        used_secrets: requirements.secrets,
        deployed_at: now - (opts.deployedAgoSecs ?? 86400 * 9),
        previous_spec: null,
      };
      for (const [cfgName, content] of Object.entries(opts.configs ?? {})) {
        configs[cfgName] = { name: cfgName, content: encodeText(content), revision: 1, owner: name };
      }
      for (const s of opts.secrets ?? []) {
        secrets[s] = { name: s, sealed: '', revision: 1, key_fingerprint: fakeDigest(s).slice(7, 23), owner: name };
      }
      for (const svc of parsed.services) {
        const placed = opts.placement?.[svc.name] ?? svc.node;
        for (let i = 0; i < svc.replicas; i++) {
          const id = `${name}.${svc.name}.${i}`;
          const pick = Array.isArray(placed) ? placed[i] : placed;
          const node = pick ?? upNodes[rr++ % Math.max(upNodes.length, 1)] ?? this.nodeInputs[0].name;
          const digest = fakeDigest(svc.image);
          assignments[id] = { instance: id, node, desired: 'running', image_digest: digest, migration: null };
          live[id] = {
            image: svc.image,
            running_digest: digest,
            started_at: new Date((now - 3600 * (20 + rr * 5)) * 1000).toISOString(),
            exit_code: null,
            oom_killed: false,
            restart_limit: 5,
            memory_limit_bytes: null,
            health: 'healthy',
            ...(svc.proxy[0] && {
              proxy: {
                host: svc.proxy[0].host.includes('.') ? svc.proxy[0].host : `${svc.proxy[0].host}.${domains[svc.proxy[0].network] ?? 'example.com'}`,
                checked_at: new Date(now * 1000).toISOString(),
              },
            }),
            ...opts.live?.[id],
          };
          const update = opts.updateAvailable?.includes(svc.name);
          updates.push({
            id,
            stack: name,
            service: svc.name,
            node,
            image: svc.image,
            running_digest: digest,
            latest_digest: update ? fakeDigest(`${svc.image}-new`) : digest,
            pinned_digest: digest,
            update_available: Boolean(update),
            pin_drift: false,
          });
          for (const vol of svc.volumes) {
            const drive = vol.split('/')[0];
            const implementers = this.nodeInputs.filter((n) => !n.down && (n.drives ?? { [drive]: 1 })[drive] !== undefined);
            const replicated = this.drives[drive]?.mode !== 'served';
            volumes[vol] ??= {
              id: vol,
              generation: 3,
              primary: node,
              replicas: Object.fromEntries(
                (replicated ? implementers.map((n) => n.name) : [node]).map((n) => [n, 3]),
              ),
              frozen: null,
              sync_refused: null,
              looks_empty: false,
            };
          }
        }
        for (const p of svc.proxy) {
          const host = p.host.includes('.') ? p.host : `${p.host}.${domains[p.network] ?? 'example.com'}`;
          const owner = assignments[`${name}.${svc.name}.0`].node;
          const ip = this.nodeInputs.find((n) => n.name === owner)?.ip ?? '192.0.2.1';
          proxyRoutes.push({
            host,
            path: p.path_prefix ?? '/',
            kind: 'service',
            stack: name,
            service: svc.name,
            port: p.port,
            network: p.network,
            tls: true,
            websockets: true,
            apex: false,
            cert_not_after: now + 86400 * 61,
            health: { node: owner, checked_at: new Date(now * 1000).toISOString(), healthy: true, error: null },
          });
          certificates[host] = {
            fqdn: host,
            chain_pem: '',
            key_pem: '',
            issued_at: now - 86400 * 29,
            not_after: now + 86400 * 61,
          };
          dnsRecords[`${host}|A|${ip}`] = { fqdn: host, record_type: 'A', content: ip, zone: domains[p.network], provider_id: null };
        }
      }
    }

    const certStatus: MockClusterState['cert_status'] = {};
    const dnsStatus: MockClusterState['dns_status'] = {};

    const state: MockClusterState = {
      nodes,
      stacks,
      assignments,
      volumes,
      configs,
      secrets,
      certificates,
      dns_records: dnsRecords,
      cert_status: certStatus,
      dns_status: dnsStatus,
      unreachable: down,
      removed: [],
      overlay: null,
      wrapped_keys: Object.fromEntries(
        this.nodeInputs.map((n) => [
          n.name,
          { ephemeral_public: 'eph', sealed: 'sealed', key_fingerprint: fakeDigest('key').slice(7, 23), target_pubkey: `pk-${n.name}` },
        ]),
      ),
      cluster_config: null,
      emergency_stack: null,
      restic_destinations: {},
      restic_targets: {},
      restic_runs: {},
      restore_requests: {},
      instance_updates: {},
      auto_updates: {},
      image_checks: {},
      image_checks_at: 0,
      sessions: {},
      proxy_sessions: {},
      proxy_tokens: {},
      version: 100,
      alert_destinations: {},
      alert_inbox: {},
      alerts: {},
      container_status: {},
      crash_moves: {},
      previous_image_digests: {},
      volume_removals: {},
    };

    // The wire omits nulls and defaults from cluster.yaml (serde skips them),
    // which the typed interface does not model; hence the one cast.
    const config = {
      cluster: { name: this.info.cluster },
      networks: this.networks,
      drives: this.drives,
      nodes: nodeCfg,
      web: { address: 'clouduler', backend_port: 8444, network: netNames[0], port: 8443, through_proxy: true },
      data_dir: '/var/lib/clouduler',
      failover: { fence_timeout: '15s', node_timeout: '30s', probe_interval: '5s', stop_grace: '30s' },
      secrets: { generate_if_missing: true, key_file: '/etc/clouduler/secrets.key', node_key_file: '/etc/clouduler/node.key' },
      auth: { api_keys: true, session_ttl: '12h' },
      proxy: {
        acme: { enabled: true, contact_email: 'admin@example.com' },
        auth: null,
        dns_publish: 'all-nodes',
        dns_server: { enabled: false, networks: [], port: 53, upstream: [] },
        enabled: true,
        http_port: 80,
        https_port: 443,
      },
      mcp: { enabled: true },
      updates: { check_interval: '6h' },
    } as unknown as ClusterConfig;

    const names = this.nodeInputs.map((n) => n.name);
    const db: MockDb = {
      scenario: this.info.name,
      description: this.info.description,
      identity: { subject: 'mock-admin', username: 'demo', role: 'admin', mcp_enabled: true },
      state,
      config,
      usage,
      live,
      pendingInstances: [],
      updates,
      proxyRoutes,
      redirects: [],
      staticRoutes: [],
      wireguardIngress: [],
      registries: [],
      apiKeys: [{ id: 'key-1', name: 'laptop', created_at: new Date((now - 86400 * 20) * 1000).toISOString() }],
      mcpClients: [],
      oauthRequests: [],
      alerts: {
        active: this.alertViews.filter((a) => a.state === 'active'),
        recent: this.alertViews.filter((a) => a.state !== 'active'),
        counts: {
          critical: this.alertViews.filter((a) => a.state === 'active' && a.severity === 'critical').length,
          warning: this.alertViews.filter((a) => a.state === 'active' && a.severity === 'warning').length,
          info: this.alertViews.filter((a) => a.state === 'active' && a.severity === 'info').length,
        },
      },
      alertCatalog: defaultAlertCatalog(),
      alertSettings: defaultAlertSettings(),
      backupDestinations: [],
      backupRuns: [],
      snapshots: [],
      restoreRequests: [],
      selfUpdate: {
        repo: null,
        latest: { tag: 'v0.9.10', architectures: ['x86_64', 'aarch64'], source: 'github.com/example/clouduler', running_version: '0.9.10', fetched_at: now },
        run: null,
        githubTokenSet: false,
      },
      gitBackup: { configured: false, deploy_public_key: null, last_export: null, last_pull: null },
      gitWebhookSecretSet: false,
      cloudflareTokenSet: false,
      oidcSecretSet: false,
      proxyAuthSecretSet: false,
      clusterKey: { fingerprint: fakeDigest('key').slice(7, 23), held: true },
      pending: { armed: false, seconds_left: null },
      lastRebalance: null,
      connectivity: {
        reports: this.nodeInputs.map((n) => ({
          node: n.name,
          reported: !n.down,
          disk_ms: n.down ? null : 4,
          links: n.down
            ? []
            : names
                .filter((p) => p !== n.name)
                .map((peer) => {
                  const peerDown = this.nodeInputs.find((x) => x.name === peer)?.down;
                  const cross = n.site && this.nodeInputs.find((x) => x.name === peer)?.site !== n.site;
                  return { peer, raft: !peerDown, api: !peerDown, raft_ms: peerDown ? null : cross ? 38 : 1 };
                }),
        })),
      },
      tlsFingerprint: fakeDigest('tls').slice(7, 71),
      now,
      listeners: new Set(),
      nextId: 1000,
    };
    this.tweaks.forEach((t) => t(db));
    return db;
  }
}

export function migrationTo(to: string, phase: Migration['phase'] = 'syncing'): Migration {
  return { to, phase };
}

export function defaultAlertCatalog(): AlertCatalogEntry[] {
  const e = (key: string, category: string, title: string, severity: AlertCatalogEntry['severity'], stateful = true): AlertCatalogEntry => ({
    key,
    category,
    title,
    severity,
    note: '',
    stateful,
    stack_scoped: category === 'stacks',
    override: null,
    effective_severity: severity,
    routes: ['in-app'],
  });
  return [
    e('cluster.quorum_lost', 'cluster', 'Quorum lost', 'critical'),
    e('node.unreachable', 'nodes', 'Node unreachable', 'critical'),
    e('node.disk_low', 'nodes', 'Disk almost full', 'warning'),
    e('instance.crash_loop', 'stacks', 'Container crash loop', 'critical'),
    e('cert.expiring', 'certificates', 'Certificate expiring', 'warning'),
    e('backup.failed', 'backups', 'Backup failed', 'warning', false),
  ];
}

export function defaultAlertSettings(): AlertSettingsResponse {
  return {
    settings: {
      enabled: true,
      warning_debounce: '5m',
      digest_at: '09:00',
      renotify_on_escalation: true,
      notify_resolved: true,
      display_pending_warnings: false,
      retention: '30days',
      destinations: [{ name: 'ops', kind: 'webhook', enabled: true, severities: ['critical', 'warning'], digest: false }],
      events: {},
      thresholds: {
        disk_warning_percent: 90,
        disk_critical_percent: 95,
        memory_percent: 92,
        memory_for: '10m',
        cert_warning_days: 14,
        cert_critical_days: 7,
      },
    },
    secrets: { ops: { url: 'https://hooks.example.com/…', auth_set: false } },
    smtp_password: false,
    health: { ops: { ok: true, at: Math.floor(Date.now() / 1000) - 600, consecutive_failures: 0 } },
  };
}

export { parseSpec };
export type { DnsRecordState };
