import type { ClusterState, StackRequirements } from '../api/types';
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

/** Why each node can or cannot run this stack's services: one row per thing a
 *  placement needs (not drained, a network, a drive, a config, a secret), one
 *  column per node. Missing configs/secrets get a create action inline, since
 *  that's the one gap a person can fix from right here. */
export function ConstraintTable({
  requirements,
  cluster,
  servedDrives,
  natNetworks,
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
   *  which isn't the real constraint. */
  servedDrives: Set<string>;
  /** Names of networks of kind `nat`: unlike an ordinary network, or an
   *  `egress`/`wireguard` interface, a nat network takes no per-node config
   *  at all, so every node implements it automatically. Mirrors the
   *  scheduler's `everywhere_networks` bypass in `cl-sched::place::rejection`
   *  - without it this table would show every node as unable to run a
   *  service on a nat network, since none of them ever appear in
   *  `node.networks`. */
  natNetworks: Set<string>;
  onCreateConfig: (name: string) => void;
  onCreateSecret: (name: string) => void;
}) {
  const nodes = Object.values(cluster.nodes).sort((a, b) => a.name.localeCompare(b.name));

  const rows: Row[] = [
    { label: 'not drained', ok: (n) => cluster.nodes[n]?.availability === 'active' },
    ...requirements.networks.map((net) => ({
      label: `network: ${net}`,
      ok: (n: string) => natNetworks.has(net) || (cluster.nodes[n]?.networks.includes(net) ?? false),
    })),
    ...requirements.drives.map((drive) => ({
      label: `drive: ${drive}`,
      ok: (n: string) =>
        servedDrives.has(drive) || Object.prototype.hasOwnProperty.call(cluster.nodes[n]?.drives ?? {}, drive),
    })),
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
