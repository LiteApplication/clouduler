// Managed private networks (networks.NET.managed: true): clouduler builds one
// cluster-wide VXLAN overlay over the subnet between the nodes that list the
// network. Every such node holds a node address on it (explicit, or null =
// automatic from the top of the subnet; containers fill the pool from the
// bottom) and outside clients reach containers only through those addresses.
// These helpers mirror ClusterConfig::managed_node_addresses and
// managed_reserved (cl-config cluster.rs) and the managed rules of
// cl-config validate.rs, so the form can flag mistakes before a save
// round-trips. The server stays the authority.

import type { ClusterConfig, NetworkConfig } from '../api/types';
import { isValidIpv4 } from './validate';

export const MANAGED_MIN_PREFIX = 16;
export const MANAGED_MAX_PREFIX = 29;

export interface Cidr4 {
  /** Network address as an unsigned 32 bit integer. */
  base: number;
  prefix: number;
}

export function ipv4ToInt(ip: string): number | null {
  if (!isValidIpv4(ip)) return null;
  return ip.split('.').reduce((acc, o) => acc * 256 + Number(o), 0);
}

export function intToIpv4(n: number): string {
  return [24, 16, 8, 0].map((s) => Math.floor(n / 2 ** s) % 256).join('.');
}

/** Parses an IPv4 CIDR. `addr` keeps the host bits the user typed. */
export function parseCidr4(cidr: string): (Cidr4 & { addr: number }) | null {
  const [a, p] = cidr.split('/');
  const addr = ipv4ToInt(a ?? '');
  if (addr === null || p === undefined || !/^\d+$/.test(p)) return null;
  const prefix = Number(p);
  if (prefix > 32) return null;
  const size = 2 ** (32 - prefix);
  return { base: Math.floor(addr / size) * size, prefix, addr };
}

const size4 = (c: Cidr4) => 2 ** (32 - c.prefix);
const contains4 = (c: Cidr4, ip: number) => ip >= c.base && ip < c.base + size4(c);
const overlaps4 = (a: Cidr4, b: Cidr4) => contains4(a, b.base) || contains4(b, a.base);

/** Whether `net` is a managed network (private kind, like Network::is_managed). */
export function isManaged(net: NetworkConfig | undefined): boolean {
  return !!net && net.managed === true && net.type === 'private';
}

/** The IPv4 subnet of a managed network, or null. */
export function managedSubnet(net: NetworkConfig | undefined): Cidr4 | null {
  if (!net || !isManaged(net) || !net.subnet) return null;
  return parseCidr4(net.subnet);
}

/** First host address, the anycast gateway. */
export function managedGateway(c: Cidr4): number {
  return c.base + 1;
}

export function managedNetworkNames(cfg: ClusterConfig): string[] {
  return Object.entries(cfg.networks)
    .filter(([, n]) => isManaged(n))
    .map(([name]) => name);
}

/**
 * Nodes holding an IPv4 address on `network` (ClusterConfig::addressed_nodes_on).
 * On a managed network a node that lists it with `null` has an automatic address.
 */
export function addressedNodesOn(cfg: ClusterConfig, network: string): string[] {
  const managed = isManaged(cfg.networks[network]);
  return Object.entries(cfg.nodes)
    .filter(([, node]) => {
      const a = node.network[network];
      return (typeof a === 'string' && a !== '') || (managed && a === null);
    })
    .map(([name]) => name);
}

/** FNV-1a 64 bit, like cl-config netalloc::fnv1a. */
function fnv1a(s: string): bigint {
  const mask = (1n << 64n) - 1n;
  let h = 0xcbf29ce484222325n;
  for (const b of new TextEncoder().encode(s)) {
    h ^= BigInt(b);
    h = (h * 0x100000001b3n) & mask;
  }
  return h;
}

/**
 * Address of every node that lists managed network `network`
 * (ClusterConfig::managed_node_addresses): explicit ones as typed, `null`
 * ones automatic from the top of the subnet downward, picked by a stable hash
 * of the node name and probed past the gateway and addresses already taken.
 * A node the subnet has no room for is absent.
 */
export function managedNodeAddresses(cfg: ClusterConfig, network: string): Map<string, string> {
  const out = new Map<string, string>();
  const c = managedSubnet(cfg.networks[network]);
  if (!c) return out;
  const first = c.base;
  const last = c.base + size4(c) - 1;
  const taken = new Set<number>([first, last, managedGateway(c)]);
  const auto: string[] = [];
  for (const name of Object.keys(cfg.nodes).sort()) {
    const a = cfg.nodes[name].network[network];
    if (typeof a === 'string' && a !== '') {
      const n = ipv4ToInt(a);
      if (n !== null) {
        taken.add(n);
        out.set(name, a);
      }
    } else if (a === null) {
      auto.push(name);
    }
  }
  const hosts = Math.max(last - first - 1, 0);
  for (const name of auto) {
    const start = hosts === 0 ? 0 : Number(fnv1a(name) % BigInt(hosts));
    for (let i = 0; i < hosts; i++) {
      const ip = last - 1 - ((start + i) % hosts);
      if (!taken.has(ip)) {
        taken.add(ip);
        out.set(name, intToIpv4(ip));
        break;
      }
    }
  }
  return out;
}

/** The address `node` holds on managed network `network`, or null. */
export function nodeAddressOn(cfg: ClusterConfig, node: string, network: string): string | null {
  return managedNodeAddresses(cfg, network).get(node) ?? null;
}

/**
 * Why `network` cannot carry the dashboard or the login portal, or null: a
 * managed network is only usable when at least one node has an address on it,
 * since those pages are served from node addresses (validate_web and
 * validate_proxy in cl-config validate.rs).
 */
export function managedPortalError(cfg: ClusterConfig, network: string, what: string): string | null {
  if (!isManaged(cfg.networks[network])) return null;
  if (addressedNodesOn(cfg, network).length > 0) return null;
  return `${network} is a managed network and no node lists it; ${what} is served from node addresses, so add the network to at least one node`;
}

/** Network, broadcast, gateway and every node address (explicit or automatic) on `network`. */
export function managedReserved(cfg: ClusterConfig, network: string): Set<number> {
  const out = new Set<number>();
  const c = managedSubnet(cfg.networks[network]);
  if (!c) return out;
  out.add(c.base);
  out.add(c.base + size4(c) - 1);
  out.add(managedGateway(c));
  for (const a of managedNodeAddresses(cfg, network).values()) {
    const n = ipv4ToInt(a);
    if (n !== null) out.add(n);
  }
  return out;
}

/** Problem with a managed network's own settings, or null. */
export function managedNetworkError(cfg: ClusterConfig, name: string): string | null {
  const net = cfg.networks[name];
  if (!net?.managed) {
    if (net?.proxy) return 'proxy is only allowed on a managed network';
    if (net?.open_ports?.length) return 'open_ports is only allowed on a managed network';
    return null;
  }
  if (net.type !== 'private') return `only a private network can be managed, this one is ${net.type}`;
  if (!net.subnet) return 'a managed network needs an IPv4 subnet';
  const c = parseCidr4(net.subnet);
  if (!c) return net.subnet.includes(':') ? 'a managed network needs an IPv4 subnet' : 'not a valid IPv4 CIDR';
  if (c.prefix < MANAGED_MIN_PREFIX || c.prefix > MANAGED_MAX_PREFIX) {
    return `the prefix length of a managed network must be between /${MANAGED_MIN_PREFIX} and /${MANAGED_MAX_PREFIX}`;
  }
  if (c.addr !== c.base) return `${net.subnet} has host bits set, did you mean ${intToIpv4(c.base)}/${c.prefix}?`;
  const pools: [number, string][] = [
    [244, 'the overlay pool 10.244.0.0/16'],
    [245, 'the egress pool 10.245.0.0/16'],
    [246, 'the nat pool 10.246.0.0/16'],
  ];
  for (const [octet, what] of pools) {
    if (overlaps4(c, { base: (10 * 256 + octet) * 65536, prefix: 16 })) return `${net.subnet} overlaps ${what}`;
  }
  for (const [other, o] of Object.entries(cfg.networks)) {
    if (other === name || !o.subnet) continue;
    const oc = parseCidr4(o.subnet);
    if (!oc) continue;
    // A container network routing a wider range is fine: the managed subnet is more specific.
    if (o.type === 'container' && contains4(oc, c.base) && !(oc.base === c.base && oc.prefix === c.prefix)) continue;
    if (overlaps4(c, oc)) return `${net.subnet} overlaps the subnet ${o.subnet} of network ${other}`;
  }
  return managedOpenPortsError(net) ?? managedRoomError(cfg, name, c);
}

/** Problem with a managed network's open_ports list, or null (validate.rs). */
export function managedOpenPortsError(net: NetworkConfig): string | null {
  const seen = new Set<string>();
  for (const p of net.open_ports ?? []) {
    const id = `${p.port}/${p.protocol}`;
    if (!p.port || p.target_port === 0) return 'port 0 is not valid';
    if (seen.has(id)) return `${id} is listed more than once`;
    seen.add(id);
    if (!p.stack.trim() || !p.service.trim()) return `${id} must name the stack and service it maps to`;
    if (net.proxy && p.protocol === 'tcp' && (p.port === 80 || p.port === 443)) {
      return `${id} is served by the reverse proxy (proxy: true) and cannot also be an open port`;
    }
  }
  return null;
}

/** Every implementing node needs room for an address next to the gateway, and the pool some for containers. */
function managedRoomError(cfg: ClusterConfig, name: string, c: Cidr4): string | null {
  const implementing = Object.values(cfg.nodes).filter((n) => name in n.network).length;
  const hosts = size4(c) - 2;
  if (implementing + 1 >= hosts) {
    return `${intToIpv4(c.base)}/${c.prefix} is too small for the gateway, ${implementing} node addresses and any containers`;
  }
  return null;
}

/** Problem with node `node`'s explicit address on managed network `network`, or null. Automatic (null) addresses cannot be wrong. */
export function managedNodeAddressError(cfg: ClusterConfig, node: string, network: string): string | null {
  const c = managedSubnet(cfg.networks[network]);
  const addr = cfg.nodes[node]?.network[network];
  if (!c || typeof addr !== 'string') return null;
  if (addr === '') return 'enter an address or tick automatic';
  const ip = ipv4ToInt(addr);
  if (ip === null) return 'a managed network takes an IPv4 address';
  if (!contains4(c, ip)) return `${addr} is outside the subnet ${intToIpv4(c.base)}/${c.prefix}`;
  if (ip === c.base || ip === c.base + size4(c) - 1) return `${addr} is the network or broadcast address`;
  if (ip === managedGateway(c)) return `${addr} is the gateway of the managed network, pick another address`;
  for (const [other, n] of Object.entries(cfg.nodes)) {
    if (other !== node && n.network[network] === addr) return `${addr} is used by node ${other}`;
  }
  return null;
}
