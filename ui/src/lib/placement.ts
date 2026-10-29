// What a placement needs and which nodes meet it. This is the one place that
// mirrors `cl_sched::place::rejection`: ConstraintTable (old views) and the
// ds PlacementTable both render the rows built here, so "can schedule here"
// always agrees with the real scheduler. Keep every check below in step with
// that function (see the constraint-table-coverage notes).

import type { ClusterState, NodeState, StackRequirements } from '../api/types';

/** Cluster context the scheduler consults beyond the node list itself. */
export interface PlacementContext {
  /** Names of drives whose mode is `served`: one authoritative copy any node
   *  can reach over the internal network, so every node is eligible for them
   *  regardless of which one declared it locally. Not consulted for a drive
   *  that also appears in `requirements.whole_drive_mounts`: see `driveHolders`. */
  servedDrives: Set<string>;
  /** Names of networks of kind `nat`: they take no per-node config at all, so
   *  every node implements them automatically. */
  natNetworks: Set<string>;
  /** Drive name -> nodes holding its real (non-mountpoint) directory: every
   *  implementing node for a replicated drive, or `served_by` for a served
   *  one. Only consulted for `requirements.whole_drive_mounts` (`drive/.`
   *  mounts), which are only accepted pinned to the drive's sole real holder. */
  driveHolders: Record<string, string[]>;
  /** Names of networks of kind `public` or `wireguard`: the only kinds a raw
   *  `x-clouduler.expose` port is checked against (`NodeState.open_ports`). */
  portGatedNetworks: Set<string>;
}

/** One thing a placement needs, with its verdict per node. */
export interface PlacementRow {
  /** Stable key; also the row label in the old table. */
  id: string;
  label: string;
  /** Secondary mono line under the label (the value being matched). */
  detail?: string;
  ok: (node: string) => boolean;
  /** Short reason shown in a failing cell. */
  reason?: (node: string) => string | undefined;
  /** True when this is a cluster wide object (config/secret) that does not
   *  exist at all yet, so a create action makes sense. */
  missing?: boolean;
  kind?: 'config' | 'secret';
  name?: string;
}

/** A parsed `deploy.placement.constraints` entry (Swarm syntax:
 *  `node.hostname==n1`, `node.labels.fast!=true`). Mirrors
 *  `cl_sched::place::Constraint::parse` exactly. */
export interface ParsedConstraint {
  key: string;
  equal: boolean;
  value: string;
}

export function parseConstraint(raw: string): ParsedConstraint | null {
  // Same precedence as the Rust parser: `!=` is checked first, so a string
  // containing both operators still resolves the same way on both sides.
  const op = raw.includes('!=') ? '!=' : raw.includes('==') ? '==' : null;
  if (!op) return null;
  const key = raw.slice(0, raw.indexOf(op)).trim();
  const value = raw.slice(raw.indexOf(op) + op.length).trim();
  if (!key || !value) return null;
  return { key, equal: op === '==', value };
}

/** Mirrors `cl_sched::place::Constraint::matches`: a label the node does not
 *  carry is a real answer (""), not a missing one, so a negated label
 *  constraint correctly includes an unlabelled node; an unknown key never
 *  matches. */
export function constraintMatches(c: ParsedConstraint, node: NodeState | undefined): boolean {
  if (!node) return false;
  if (c.key.startsWith('node.labels.')) {
    const labelKey = c.key.slice('node.labels.'.length);
    const actual = node.labels[labelKey] ?? '';
    return (actual === c.value) === c.equal;
  }
  let actual: string | null = null;
  if (c.key === 'node.hostname' || c.key === 'node.id') actual = node.name;
  else if (c.key === 'node.role') actual = node.role;
  if (actual === null) return false;
  return (actual === c.value) === c.equal;
}

/** Every check `cl_sched::place::rejection` makes, one row each. */
export function buildPlacementRows(
  requirements: StackRequirements,
  cluster: ClusterState,
  ctx: PlacementContext,
): PlacementRow[] {
  const pinnedNode = requirements.pinned_node;
  const rows: PlacementRow[] = [
    {
      id: 'reachable',
      label: 'reachable',
      ok: (n) => !cluster.unreachable.includes(n),
      reason: () => 'unreachable',
    },
    {
      id: 'not drained',
      label: 'not drained',
      ok: (n) => cluster.nodes[n]?.availability === 'active',
      reason: (n) => cluster.nodes[n]?.availability,
    },
  ];
  if (pinnedNode) {
    rows.push({
      id: `pinned to: ${pinnedNode}`,
      label: `pinned to: ${pinnedNode}`,
      ok: (n) => n === pinnedNode,
      reason: () => `pinned to ${pinnedNode}`,
    });
  }
  for (const net of requirements.networks) {
    rows.push({
      id: `network: ${net}`,
      label: `network: ${net}`,
      ok: (n) => ctx.natNetworks.has(net) || (cluster.nodes[n]?.networks.includes(net) ?? false),
      reason: () => 'not on network',
    });
  }
  for (const drive of requirements.drives) {
    if (requirements.whole_drive_mounts.includes(drive)) {
      const holders = ctx.driveHolders[drive] ?? [];
      rows.push({
        id: `drive: ${drive} (whole mount)`,
        label: `drive: ${drive} (whole mount)`,
        ok: (n) => holders.length === 1 && holders[0] === n,
        reason: () => (holders.length === 1 ? `held by ${holders[0]}` : 'no sole holder'),
      });
    } else {
      rows.push({
        id: `drive: ${drive}`,
        label: `drive: ${drive}`,
        ok: (n) =>
          ctx.servedDrives.has(drive) || Object.prototype.hasOwnProperty.call(cluster.nodes[n]?.drives ?? {}, drive),
        reason: () => 'no such drive',
      });
    }
  }
  for (const name of requirements.configs) {
    rows.push({
      id: `config: ${name}`,
      label: `config: ${name}`,
      kind: 'config',
      name,
      missing: !(name in cluster.configs),
      ok: () => name in cluster.configs,
      reason: () => 'missing',
    });
  }
  for (const name of requirements.secrets) {
    rows.push({
      id: `secret: ${name}`,
      label: `secret: ${name}`,
      kind: 'secret',
      name,
      missing: !(name in cluster.secrets),
      // A secret can only be materialized on a node that holds a wrapped copy
      // of the cluster key, so a node without one can never run it even once
      // the secret itself exists.
      ok: (n) => name in cluster.secrets && n in cluster.wrapped_keys,
      reason: (n) => (name in cluster.secrets ? (n in cluster.wrapped_keys ? undefined : 'no cluster key') : 'missing'),
    });
  }
  for (const raw of requirements.placement_constraints) {
    const parsed = parseConstraint(raw);
    rows.push({
      id: `constraint: ${raw}`,
      label: `constraint: ${raw}`,
      ok: (n) => (parsed ? constraintMatches(parsed, cluster.nodes[n]) : false),
      reason: () => (parsed ? 'no match' : 'unparseable'),
    });
  }
  for (const p of requirements.bound_ports) {
    if (!ctx.portGatedNetworks.has(p.network)) continue;
    rows.push({
      id: `port: ${p.published}/${p.protocol} open on ${p.network}`,
      label: `port: ${p.published}/${p.protocol} open on ${p.network}`,
      ok: (n) =>
        (cluster.nodes[n]?.open_ports[p.network] ?? []).some(([proto, port]) => proto === p.protocol && port === p.published),
      reason: () => 'port closed',
    });
  }
  return rows;
}

export function sortedNodes(cluster: ClusterState): NodeState[] {
  return Object.values(cluster.nodes).sort((a, b) => a.name.localeCompare(b.name));
}

/** Nodes that meet every row, and why each other node does not: the Move
 *  sheet's eligible and ineligible lists. */
export function nodeEligibility(
  rows: PlacementRow[],
  cluster: ClusterState,
): { name: string; eligible: boolean; reasons: string[] }[] {
  return sortedNodes(cluster).map((n) => {
    const failed = rows.filter((r) => !r.ok(n.name));
    return {
      name: n.name,
      eligible: failed.length === 0,
      reasons: failed.map((r) => r.reason?.(n.name) ?? r.label),
    };
  });
}
