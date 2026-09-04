import type { ClusterState, NodeState, StackRequirements } from '../api/types';
import { Badge } from './Badge';
import { Icon } from './Icon';

interface Row {
  label: string;
  ok: (node: string) => boolean;
  /** True when this is a cluster wide object (config/secret) that does not
   *  exist at all yet, so a create action makes sense. */
  missing?: boolean;
  onCreate?: () => void;
}

/** A parsed `deploy.placement.constraints` entry (Swarm syntax:
 *  `node.hostname==n1`, `node.labels.fast!=true`). Mirrors
 *  `cl_sched::place::Constraint::parse` exactly, so the table agrees with
 *  what the scheduler actually enforces. */
interface ParsedConstraint {
  key: string;
  equal: boolean;
  value: string;
}

function parseConstraint(raw: string): ParsedConstraint | null {
  // Same precedence as the Rust parser: `!=` is checked first, so a string
  // containing both operators (which should never happen in practice) still
  // resolves the same way on both sides.
  const op = raw.includes('!=') ? '!=' : raw.includes('==') ? '==' : null;
  if (!op) return null;
  const key = raw.slice(0, raw.indexOf(op)).trim();
  const value = raw.slice(raw.indexOf(op) + op.length).trim();
  if (!key || !value) return null;
  return { key, equal: op === '==', value };
}

/** Mirrors `cl_sched::place::Constraint::matches` exactly: a label the node
 *  does not carry is a real answer ("" ), not a missing one, so a negated
 *  label constraint correctly includes an unlabelled node; an unknown key
 *  never matches. */
function constraintMatches(c: ParsedConstraint, node: NodeState | undefined): boolean {
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

/** Why each node can or cannot run this stack's services: one row per thing a
 *  placement needs (reachable, not drained, a node pin, a network, a drive, a
 *  placement constraint, an exposed port, a config, a secret), one column per
 *  node. Mirrors every check `cl_sched::place::rejection` makes, so "can
 *  schedule here" here always agrees with the real scheduler. Missing
 *  configs/secrets get a create action inline, since that's the one gap a
 *  person can fix from right here. */
export function ConstraintTable({
  requirements,
  cluster,
  servedDrives,
  natNetworks,
  driveHolders,
  portGatedNetworks,
  onCreateConfig,
  onCreateSecret,
}: {
  requirements: StackRequirements;
  cluster: ClusterState;
  /** Names of drives whose mode is `served`: one authoritative copy any node
   *  can reach over the internal network, so every node is eligible for them
   *  regardless of which one declared it locally. Mirrors the scheduler's
   *  `served_drives` bypass in `cl-sched::place::rejection` - without it this
   *  table always shows a served drive's own node as the only candidate,
   *  which isn't the real constraint. Not consulted for a drive that also
   *  appears in `requirements.whole_drive_mounts`: see `driveHolders`. */
  servedDrives: Set<string>;
  /** Names of networks of kind `nat`: unlike an ordinary network, or an
   *  `egress`/`wireguard` interface, a nat network takes no per-node config
   *  at all, so every node implements it automatically. Mirrors the
   *  scheduler's `everywhere_networks` bypass in `cl-sched::place::rejection`
   *  - without it this table would show every node as unable to run a
   *  service on a nat network, since none of them ever appear in
   *  `node.networks`. */
  natNetworks: Set<string>;
  /** Drive name -> nodes holding its real (non-mountpoint) directory: every
   *  implementing node for a replicated drive, or `served_by` for a served
   *  one. Only consulted for a drive in `requirements.whole_drive_mounts`
   *  (a `drive/.` mount), which `validate_whole_drive_mounts` only accepts
   *  pinned to that drive's sole real holder - a stricter bar than the
   *  ordinary implements-or-served check `servedDrives` gives every other
   *  drive mount. */
  driveHolders: Record<string, string[]>;
  /** Names of networks of kind `public` or `wireguard`: the only kinds a
   *  raw `x-clouduler.expose` port is actually checked against
   *  (`NodeState.open_ports`), matching `ClusterConfig::port_gated_networks`.
   *  A `bound_ports` entry on any other network kind is never a real
   *  placement constraint and gets no row. */
  portGatedNetworks: Set<string>;
  onCreateConfig: (name: string) => void;
  onCreateSecret: (name: string) => void;
}) {
  const nodes = Object.values(cluster.nodes).sort((a, b) => a.name.localeCompare(b.name));
  const pinnedNode = requirements.pinned_node;

  const rows: Row[] = [
    { label: 'reachable', ok: (n) => !cluster.unreachable.includes(n) },
    { label: 'not drained', ok: (n) => cluster.nodes[n]?.availability === 'active' },
    ...(pinnedNode ? [{ label: `pinned to: ${pinnedNode}`, ok: (n: string) => n === pinnedNode }] : []),
    ...requirements.networks.map((net) => ({
      label: `network: ${net}`,
      ok: (n: string) => natNetworks.has(net) || (cluster.nodes[n]?.networks.includes(net) ?? false),
    })),
    ...requirements.drives.map((drive) => {
      if (requirements.whole_drive_mounts.includes(drive)) {
        const holders = driveHolders[drive] ?? [];
        return {
          label: `drive: ${drive} (whole mount)`,
          ok: (n: string) => holders.length === 1 && holders[0] === n,
        };
      }
      return {
        label: `drive: ${drive}`,
        ok: (n: string) =>
          servedDrives.has(drive) || Object.prototype.hasOwnProperty.call(cluster.nodes[n]?.drives ?? {}, drive),
      };
    }),
    ...requirements.configs.map((name) => ({
      label: `config: ${name}`,
      missing: !(name in cluster.configs),
      onCreate: () => onCreateConfig(name),
      ok: () => name in cluster.configs,
    })),
    ...requirements.secrets.map((name) => ({
      label: `secret: ${name}`,
      missing: !(name in cluster.secrets),
      onCreate: () => onCreateSecret(name),
      // A secret can only be materialized on a node that holds a wrapped
      // copy of the cluster key, so a node without one can never run it even
      // once the secret itself exists.
      ok: (n: string) => name in cluster.secrets && n in cluster.wrapped_keys,
    })),
    ...requirements.placement_constraints.flatMap((raw) => {
      const parsed = parseConstraint(raw);
      return [{ label: `constraint: ${raw}`, ok: (n: string) => (parsed ? constraintMatches(parsed, cluster.nodes[n]) : false) }];
    }),
    ...requirements.bound_ports
      .filter((p) => portGatedNetworks.has(p.network))
      .map((p) => ({
        label: `port: ${p.published}/${p.protocol} open on ${p.network}`,
        ok: (n: string) =>
          (cluster.nodes[n]?.open_ports[p.network] ?? []).some(([proto, port]) => proto === p.protocol && port === p.published),
      })),
  ];

  if (nodes.length === 0) {
    return <p className="muted">no nodes in the cluster</p>;
  }

  return (
    <div className="table-wrap">
      <table>
        <thead>
          <tr>
            <th>Constraint</th>
            {nodes.map((n) => (
              <th key={n.name}>{n.name}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.label}>
              <td>
                {row.label}
                {row.missing && row.onCreate && (
                  <button type="button" onClick={row.onCreate} style={{ marginLeft: 8 }}>
                    <Icon name="plus" /> Create
                  </button>
                )}
              </td>
              {nodes.map((n) => (
                <td key={n.name}>
                  {row.ok(n.name) ? (
                    <Icon name="check" />
                  ) : (
                    <span className="muted">
                      <Icon name="close" />
                    </span>
                  )}
                </td>
              ))}
            </tr>
          ))}
          <tr>
            <td>
              <strong>can schedule here</strong>
            </td>
            {nodes.map((n) => {
              const can = rows.every((r) => r.ok(n.name));
              return (
                <td key={n.name}>
                  <Badge tone={can ? 'good' : 'bad'}>{can ? 'yes' : 'no'}</Badge>
                </td>
              );
            })}
          </tr>
        </tbody>
      </table>
    </div>
  );
}
