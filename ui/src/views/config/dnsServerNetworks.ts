// Pure helpers for the per-network "Serve cluster DNS" switch. The config
// stays a single list (`proxy.dns_server.networks`); empty means the default set.

import type { NetworkConfig } from '../../api/types';

type Networks = Record<string, NetworkConfig>;

/** Kinds that can listen for DNS at all. */
export function canServeDns(net: NetworkConfig): boolean {
  return net.type === 'internal' || net.type === 'private' || net.type === 'public' || net.type === 'container';
}

/** Served when the list is empty: internal, private, and container networks with `proxy: true`. */
export function defaultDnsNetworks(networks: Networks): string[] {
  return Object.entries(networks)
    .filter(([, n]) => n.type === 'internal' || n.type === 'private' || (n.type === 'container' && n.container?.proxy === true))
    .map(([name]) => name)
    .sort();
}

/** The set actually served: the list when non-empty, else the default set. */
export function effectiveDnsNetworks(networks: Networks, list: string[]): string[] {
  const valid = list.filter((n) => n in networks);
  return valid.length > 0 ? [...valid].sort() : defaultDnsNetworks(networks);
}

export function isServingDns(networks: Networks, list: string[], name: string): boolean {
  return effectiveDnsNetworks(networks, list).includes(name);
}

/** True when switching `name` off would leave nothing served (not expressible: empty = default). */
export function isLastServed(networks: Networks, list: string[], name: string): boolean {
  const eff = effectiveDnsNetworks(networks, list);
  return eff.length === 1 && eff[0] === name;
}

/** New value of `proxy.dns_server.networks` after switching `name` on or off.
 *  Returns null when the change cannot be expressed (turning off the last served network). */
export function toggleDnsNetwork(networks: Networks, list: string[], name: string, on: boolean): string[] | null {
  const eff = new Set(effectiveDnsNetworks(networks, list));
  if (on) eff.add(name);
  else eff.delete(name);
  if (eff.size === 0) return null;
  const next = [...eff].sort();
  const def = defaultDnsNetworks(networks);
  if (next.length === def.length && next.every((n, i) => n === def[i])) return [];
  return next;
}

/** Rewrites the list after a network rename. */
export function renameDnsNetwork(list: string[], from: string, to: string): string[] {
  return list.map((n) => (n === from ? to : n));
}

/** Drops a removed network from the list. */
export function removeDnsNetwork(list: string[], name: string): string[] {
  return list.filter((n) => n !== name);
}
