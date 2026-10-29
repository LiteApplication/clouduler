import type { ClusterState, StackRequirements } from '../api/types';
import { buildPlacementRows, sortedNodes } from '../lib/placement';
import { Badge } from './Badge';
import { Icon } from './Icon';

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
  const nodes = sortedNodes(cluster);
  const rows = buildPlacementRows(requirements, cluster, { servedDrives, natNetworks, driveHolders, portGatedNetworks });

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
            <tr key={row.id}>
              <td>
                {row.label}
                {row.missing && row.kind && row.name && (
                  <button
                    type="button"
                    onClick={() => (row.kind === 'config' ? onCreateConfig(row.name!) : onCreateSecret(row.name!))}
                    style={{ marginLeft: 8 }}
                  >
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
