// Pure helpers for the network access graph: which networks may have an
// outgoing edge, which may be a target, and the implicit public edges.

import type { NetworkConfig, NetworkKind } from '../../api/types';

type Networks = Record<string, NetworkConfig>;

/** Only these kinds carry an `access` list (the source side of an edge). */
export function canHaveAccess(kind: NetworkKind): boolean {
  return kind === 'internal' || kind === 'private' || kind === 'container';
}

/** Kinds an edge may point at. */
export function isAccessTarget(kind: NetworkKind): boolean {
  return kind === 'internal' || kind === 'private' || kind === 'public' || kind === 'container';
}

export function validTargets(networks: Networks, from: string): string[] {
  return Object.entries(networks)
    .filter(([n, c]) => n !== from && isAccessTarget(c.type))
    .map(([n]) => n)
    .sort();
}

/** Explicit edges [from, to]: `to` is listed in `networks[from].access`. */
export function explicitEdges(networks: Networks): [string, string][] {
  const out: [string, string][] = [];
  for (const [from, net] of Object.entries(networks)) {
    if (!canHaveAccess(net.type)) continue;
    for (const to of net.access ?? []) if (to !== from && to in networks) out.push([from, to]);
  }
  return out;
}

/** Public networks every other source network reaches without listing them. */
export function implicitPublicEdges(networks: Networks): [string, string][] {
  const explicit = new Set(explicitEdges(networks).map(([a, b]) => `${a}\0${b}`));
  const out: [string, string][] = [];
  for (const [from, net] of Object.entries(networks)) {
    if (!canHaveAccess(net.type)) continue;
    for (const [to, t] of Object.entries(networks)) {
      if (to !== from && t.type === 'public' && !explicit.has(`${from}\0${to}`)) out.push([from, to]);
    }
  }
  return out;
}

export function reachedBy(networks: Networks, name: string): string[] {
  return explicitEdges(networks).filter(([, b]) => b === name).map(([a]) => a).sort();
}

/** Everything `name` can reach: its explicit list plus implicit public networks. */
export function canReach(networks: Networks, name: string): { explicit: string[]; implicit: string[]; unrestricted: boolean } {
  const net = networks[name];
  if (!net || !canHaveAccess(net.type)) return { explicit: [], implicit: [], unrestricted: false };
  const explicit = explicitEdges(networks).filter(([a]) => a === name).map(([, b]) => b).sort();
  const implicit = implicitPublicEdges(networks).filter(([a]) => a === name).map(([, b]) => b).sort();
  return { explicit, implicit, unrestricted: net.access == null };
}
