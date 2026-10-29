import { useState, type ReactNode } from 'react';
import { buildPlacementRows, sortedNodes, type PlacementContext, type PlacementRow } from '../lib/placement';
import type { ClusterState, StackRequirements } from '../api/types';
import { Icon } from './Icon';
import { StatusBadge } from './StatusBadge';
import { cx } from './cx';

export interface PlacementServer {
  id: string;
  name: string;
  address?: string;
}

export interface PlacementConstraint {
  id: string;
  label: string;
  detail?: string;
  results: Record<string, { ok: boolean; reason?: string }>;
  /** Extra content in the label cell, e.g. a "Create" button for a missing secret. */
  action?: ReactNode;
}

/** Adapts the scheduler-mirroring rows from lib/placement.ts to the table. */
export function placementFromRows(
  rows: PlacementRow[],
  cluster: ClusterState,
  action?: (row: PlacementRow) => ReactNode,
): { servers: PlacementServer[]; constraints: PlacementConstraint[] } {
  const nodes = sortedNodes(cluster);
  return {
    servers: nodes.map((n) => ({ id: n.name, name: n.name, address: n.rpc_addr })),
    constraints: rows.map((r) => ({
      id: r.id,
      label: r.label,
      detail: r.detail,
      action: action?.(r),
      results: Object.fromEntries(
        nodes.map((n) => {
          const ok = r.ok(n.name);
          return [n.name, { ok, reason: ok ? undefined : r.reason?.(n.name) }];
        }),
      ),
    })),
  };
}

export function PlacementTable({
  servers,
  constraints,
  picked,
  caption,
  label = 'Placement constraints',
  defaultOpen,
  className,
}: {
  servers: PlacementServer[];
  constraints: PlacementConstraint[];
  picked?: string;
  caption?: string;
  label?: string;
  defaultOpen?: boolean;
  className?: string;
}) {
  const [open, setOpen] = useState(!!defaultOpen);
  const met = (c: PlacementConstraint, s: PlacementServer) => !!c.results[s.id]?.ok;
  const everywhere = constraints.filter((c) => servers.every((s) => met(c, s)));
  const rest = constraints.filter((c) => !everywhere.includes(c));
  const blocked = (s: PlacementServer) => constraints.filter((c) => !met(c, s)).length;

  const row = (c: PlacementConstraint, muted: boolean) => (
    <tr key={c.id} className={muted ? 'cl-pt__row--muted' : undefined}>
      <th scope="row" className="cl-pt__con">
        <span className="cl-pt__name">{c.label}</span>
        {c.detail && <span className="cl-code-sm cl-muted">{c.detail}</span>}
        {c.action}
      </th>
      {servers.map((s) => {
        const ok = met(c, s);
        const reason = c.results[s.id]?.reason;
        return (
          <td key={s.id} className={cx('cl-pt__cell', ok ? 'cl-pt__cell--ok' : 'cl-pt__cell--bad')}>
            <Icon name={ok ? 'check-circle' : 'x'} size={18} />
            <span className="cl-sr">{ok ? 'Met' : 'Not met'}</span>
            {reason && !ok && <span className="cl-pt__reason cl-code-sm">{reason}</span>}
          </td>
        );
      })}
    </tr>
  );

  const n = everywhere.length;
  const foldLabel = `${n} constraint${n === 1 ? '' : 's'} met on all ${servers.length} servers`;
  return (
    <div className={cx('cl-pt', className)} role="region" aria-label={label} tabIndex={0}>
      <table className="cl-pt__table">
        {caption && <caption className="cl-sr">{caption}</caption>}
        <thead>
          <tr>
            <th scope="col" className="cl-pt__con cl-label">
              Constraint
            </th>
            {servers.map((s) => (
              <th key={s.id} scope="col" className="cl-pt__srv">
                <span className="cl-pt__srvname">{s.name}</span>
                {s.address && <span className="cl-code-sm cl-muted">{s.address}</span>}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rest.map((c) => row(c, false))}
          {n > 0 && (
            <tr className="cl-pt__fold">
              <td colSpan={servers.length + 1}>
                <button type="button" className="cl-pt__foldbtn" aria-expanded={open} onClick={() => setOpen(!open)}>
                  <Icon name="chevron" size={18} className={cx('cl-pt__chev', open && 'cl-pt__chev--open')} />
                  <span>{foldLabel}</span>
                </button>
              </td>
            </tr>
          )}
          {n > 0 && open && everywhere.map((c) => row(c, true))}
        </tbody>
        <tfoot>
          <tr>
            <th scope="row" className="cl-pt__con">
              <span className="cl-pt__name">Eligible servers</span>
              <span className="cl-sm cl-muted">Meets every constraint</span>
            </th>
            {servers.map((s) => {
              const b = blocked(s);
              return (
                <td key={s.id} className="cl-pt__sum">
                  {b === 0 ? (
                    <StatusBadge status="healthy" label="Eligible" />
                  ) : (
                    <StatusBadge status="failed" label={`Blocked · ${b}`} />
                  )}
                  {picked === s.id && (
                    <span className="cl-role cl-role--leader">
                      <Icon name="crown" size={14} />
                      <span>Picked</span>
                    </span>
                  )}
                </td>
              );
            })}
          </tr>
        </tfoot>
      </table>
    </div>
  );
}

/** PlacementTable fed straight from a stack's requirements, the way the old
 *  ConstraintTable was. */
export function PlacementTableFor({
  requirements,
  cluster,
  context,
  action,
  picked,
  className,
}: {
  requirements: StackRequirements;
  cluster: ClusterState;
  context: PlacementContext;
  action?: (row: PlacementRow) => ReactNode;
  picked?: string;
  className?: string;
}) {
  const rows = buildPlacementRows(requirements, cluster, context);
  const { servers, constraints } = placementFromRows(rows, cluster, action);
  return <PlacementTable servers={servers} constraints={constraints} picked={picked} className={className} />;
}
