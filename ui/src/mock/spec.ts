// A deliberately small reader for stack yaml. The real server parses the whole
// compose dialect; the mock only needs enough to turn a deployed spec into
// instances, volumes, proxy hosts and a requirements breakdown.

import type { StackRequirements } from '../api/types';

export interface ParsedService {
  name: string;
  image: string;
  replicas: number;
  /** `drive/volume` ids. */
  volumes: string[];
  networks: string[];
  secrets: string[];
  configs: string[];
  proxy: { host: string; network: string; port: number; path_prefix?: string }[];
  node: string | null;
}

export interface ParsedSpec {
  services: ParsedService[];
  networks: string[];
}

export function parseSpec(spec: string): ParsedSpec {
  const services: ParsedService[] = [];
  const networks = new Set<string>();
  let section = '';
  let svc: ParsedService | null = null;
  let sub = '';
  let proxy: ParsedService['proxy'][number] | null = null;
  for (const raw of spec.split('\n')) {
    if (!raw.trim() || raw.trim().startsWith('#')) continue;
    const indent = raw.length - raw.trimStart().length;
    const line = raw.trim();
    if (indent === 0) {
      section = line.replace(/:.*$/, '');
      svc = null;
      continue;
    }
    if (section === 'networks' && indent === 2) {
      networks.add(line.replace(/:.*$/, ''));
      continue;
    }
    if (section !== 'services') continue;
    if (indent === 2) {
      const name = line.replace(/:.*$/, '');
      svc = { name, image: '', replicas: 1, volumes: [], networks: [], secrets: [], configs: [], proxy: [], node: null };
      services.push(svc);
      sub = '';
      continue;
    }
    if (!svc) continue;
    if (indent === 4) {
      const m = line.match(/^([a-z_-]+):\s*(.*)$/);
      if (!m) continue;
      sub = m[1];
      const inline = m[2].match(/^\[(.*)\]$/);
      if (m[1] === 'image') svc.image = m[2];
      if (inline) {
        const items = inline[1].split(',').map((s) => s.trim()).filter(Boolean);
        if (sub === 'networks') items.forEach((n) => (svc!.networks.push(n), networks.add(n)));
        if (sub === 'secrets') svc.secrets.push(...items);
        if (sub === 'configs') svc.configs.push(...items);
      }
      continue;
    }
    if (indent === 6 && line === 'proxy:') sub = 'proxy';
    if (line.startsWith('replicas:')) svc.replicas = Number(line.split(':')[1]) || 1;
    if (line.startsWith('node:') && sub === 'x-clouduler') svc.node = line.split(':')[1].trim();
    const item = line.match(/^- (.+)$/);
    if (item) {
      const v = item[1].trim();
      if (sub === 'volumes') {
        const m = v.match(/^([A-Za-z0-9_-]+\/[A-Za-z0-9_.-]+):/);
        if (m) svc.volumes.push(m[1]);
      } else if (sub === 'networks') {
        svc.networks.push(v);
        networks.add(v);
      } else if (sub === 'secrets') svc.secrets.push(v);
      else if (sub === 'configs') svc.configs.push(v);
      else if (sub === 'proxy' || line.startsWith('- host:')) {
        const h = line.match(/^- host:\s*(\S+)/);
        if (h) {
          proxy = { host: h[1], network: svc.networks[0] ?? '', port: 80 };
          svc.proxy.push(proxy);
        }
      }
      continue;
    }
    if (proxy && sub === 'proxy') {
      const kv = line.match(/^([a-z_]+):\s*(\S+)/);
      if (kv?.[1] === 'network') proxy.network = kv[2];
      if (kv?.[1] === 'port') proxy.port = Number(kv[2]);
      if (kv?.[1] === 'path_prefix') proxy.path_prefix = kv[2];
    }
  }
  return { services, networks: [...networks] };
}

function requirementsOf(services: ParsedService[], networks: string[], domains: Record<string, string>, perService = false): StackRequirements {
  const uniq = (xs: string[]) => [...new Set(xs)];
  const volumes = uniq(services.flatMap((s) => s.volumes));
  return {
    networks: uniq([...networks, ...services.flatMap((s) => s.networks)]),
    drives: uniq(volumes.map((v) => v.split('/')[0])),
    configs: uniq(services.flatMap((s) => s.configs)),
    secrets: uniq(services.flatMap((s) => s.secrets)),
    file_configs: [],
    file_secrets: [],
    hosts: uniq(
      services.flatMap((s) =>
        s.proxy.map((p) => (p.host.includes('.') ? p.host : `${p.host}.${domains[p.network] ?? 'example.com'}`)),
      ),
    ),
    pinned_node: perService ? (services[0]?.node ?? null) : null,
    placement_constraints: [],
    bound_ports: [],
    whole_drive_mounts: [],
    gpu_requests: [],
    host_devices: [],
    volumes,
    allowed: perService ? {} : Object.fromEntries(uniq([...networks, ...services.flatMap((s) => s.networks)]).map((n) => [n, null])),
  };
}

export function requirementsFor(spec: string, domains: Record<string, string>) {
  const parsed = parseSpec(spec);
  const per_service = Object.fromEntries(
    parsed.services.map((s) => [s.name, requirementsOf([s], [], domains, true)]),
  );
  return { parsed, requirements: requirementsOf(parsed.services, parsed.networks, domains), per_service };
}
