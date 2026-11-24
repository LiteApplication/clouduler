// Managed private networks (networks.NET.managed: true): clouduler builds one
// cluster-wide VXLAN overlay over the subnet between the nodes that list an
// address on it. These helpers mirror ClusterConfig::{managed_reserved,
// next_free_node_address} (cl-config cluster.rs) and the managed rules of
// cl-config validate.rs, so the form can autofill and flag mistakes before a
// save round-trips. The server stays the authority.

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

/** Network, broadcast, gateway and every node address on `network`. */
export function managedReserved(cfg: ClusterConfig, network: string): Set<number> {
  const out = new Set<number>();
  const c = managedSubnet(cfg.networks[network]);
  if (!c) return out;
  out.add(c.base);
  out.add(c.base + size4(c) - 1);
  out.add(managedGateway(c));
  for (const node of Object.values(cfg.nodes)) {
    const a = node.network[network];
    if (typeof a === 'string') {
      const n = ipv4ToInt(a);
      if (n !== null) out.add(n);
    }
  }
  return out;
}

/** The lowest host address not reserved (ClusterConfig::next_free_node_address). */
export function nextFreeNodeAddress(cfg: ClusterConfig, network: string): string | null {
  const c = managedSubnet(cfg.networks[network]);
  if (!c) return null;
  const used = managedReserved(cfg, network);
  // Host range like Ipv4Net::hosts(): /31 and /32 use every address, others skip network and broadcast.
  const first = c.prefix >= 31 ? c.base : c.base + 1;
  const last = c.prefix >= 31 ? c.base + size4(c) - 1 : c.base + size4(c) - 2;
  for (let ip = first; ip <= last; ip++) if (!used.has(ip)) return intToIpv4(ip);
  return null;
}

/** Problem with a managed network's own settings, or null. */
export function managedNetworkError(cfg: ClusterConfig, name: string): string | null {
  const net = cfg.networks[name];
  if (!net?.managed) return null;
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
  return null;
}

/** Problem with node `node`'s address on managed network `network`, or null. */
export function managedNodeAddressError(cfg: ClusterConfig, node: string, network: string): string | null {
  const c = managedSubnet(cfg.networks[network]);
  const addr = cfg.nodes[node]?.network[network];
  if (!c || (addr !== null && (typeof addr !== 'string' || addr === ''))) return null;
  const internal = Object.entries(cfg.networks).find(([, n]) => n.type === 'internal')?.[0];
  const internalAddr = internal === undefined ? undefined : cfg.nodes[node]?.network[internal];
  const internalBad = typeof internalAddr === 'string' && ipv4ToInt(internalAddr) === null;
  if (addr === null) {
    return internalBad
      ? `node ${node} has no IPv4 address on the internal network ${internal}, which a managed network needs for its tunnel; give it one or remove ${network} from the node`
      : null;
  }
  const ip = ipv4ToInt(addr);
  if (ip === null) return 'a managed network takes an IPv4 address';
  if (!contains4(c, ip)) return `${addr} is outside the subnet ${intToIpv4(c.base)}/${c.prefix}`;
  if (internalBad) {
    return `node ${node} has no IPv4 address on the internal network ${internal}, which a managed network needs for its tunnel; give it one or remove its address on ${network}`;
  }
  if (ip === c.base || ip === c.base + size4(c) - 1) return `${addr} is the network or broadcast address`;
  if (ip === managedGateway(c)) return `${addr} is the gateway of the managed network, pick another address`;
  for (const [other, n] of Object.entries(cfg.nodes)) {
    if (other !== node && n.network[network] === addr) return `${addr} is used by node ${other}`;
  }
  return null;
}
