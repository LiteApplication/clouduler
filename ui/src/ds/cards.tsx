import type { ReactNode } from 'react';
import { Button } from './Button';
import { Icon, type IconName } from './Icon';
import { Meter } from './Meter';
import { StatusBadge, type Status } from './StatusBadge';
import { cx } from './cx';

const ROLE = { leader: 'Leader', follower: 'Follower', candidate: 'Candidate' } as const;
export type RaftRole = keyof typeof ROLE;

export function NodeCard({
  name,
  address,
  status = 'healthy',
  role,
  cpu,
  mem,
  disk,
  containers,
  onClick,
  className,
}: {
  name: string;
  address: string;
  status?: Status;
  role?: RaftRole;
  /** Percent values; a meter is shown only for the ones that are present. */
  cpu?: number | null;
  mem?: number | null;
  disk?: number | null;
  containers?: number | null;
  onClick?: () => void;
  className?: string;
}) {
  const body = (
    <>
      <div className="cl-node__top">
        <Icon name="node" size={22} />
        <div className="cl-node__id">
          <span className="cl-title-sm">{name}</span>
          <span className="cl-code-sm cl-muted">{address}</span>
        </div>
        <StatusBadge status={status} />
      </div>
      {role && (
        <div className={cx('cl-role', `cl-role--${role}`)}>
          {role === 'leader' && <Icon name="crown" size={14} />}
          <span>Raft {ROLE[role].toLowerCase()}</span>
        </div>
      )}
      {(cpu != null || mem != null || disk != null) && (
        <div className="cl-node__meters">
          {cpu != null && <Meter label="CPU" value={cpu} />}
          {mem != null && <Meter label="Memory" value={mem} />}
          {disk != null && <Meter label="Disk" value={disk} />}
        </div>
      )}
      {containers != null && (
        <div className="cl-node__foot cl-muted">
          <Icon name="container" size={16} />
          <span>{containers} containers</span>
        </div>
      )}
    </>
  );
  return onClick ? (
    <button type="button" className={cx('cl-card', 'cl-node', 'cl-card--press', className)} onClick={onClick}>
      {body}
    </button>
  ) : (
    <div className={cx('cl-card', 'cl-node', className)}>{body}</div>
  );
}

const COUNTS: [string, string, IconName][] = [
  ['running', 'ok', 'check-circle'],
  ['degraded', 'warn', 'alert'],
  ['failed', 'danger', 'x'],
  ['stopped', 'muted', 'pause'],
];

export function StackCard({
  name,
  services,
  nodes,
  status = 'running',
  counts = {},
  updated,
  onClick,
  className,
}: {
  name: string;
  services?: number;
  nodes?: number;
  status?: Status;
  counts?: Partial<Record<'running' | 'degraded' | 'failed' | 'stopped', number>>;
  updated?: string;
  onClick?: () => void;
  className?: string;
}) {
  const sub = (services != null ? `${services} services` : '') + (nodes != null ? ` · ${nodes} nodes` : '');
  const body = (
    <>
      <div className="cl-stack__top">
        <Icon name="stack" size={22} />
        <div className="cl-stack__id">
          <span className="cl-title-sm">{name}</span>
          <span className="cl-muted cl-sm">{sub}</span>
        </div>
        <StatusBadge status={status} />
      </div>
      <ul className="cl-stack__counts" aria-label="Containers by state">
        {COUNTS.filter(([k]) => counts[k as keyof typeof counts] != null).map(([k, tone, icon]) => (
          <li key={k} className={`cl-count cl-count--${tone}`}>
            <Icon name={icon} size={14} />
            <span className="cl-code-sm">{counts[k as keyof typeof counts]}</span>
            <span className="cl-sm">{k}</span>
          </li>
        ))}
      </ul>
      {updated && <div className="cl-muted cl-sm">{updated}</div>}
    </>
  );
  return onClick ? (
    <button type="button" className={cx('cl-card', 'cl-stack', 'cl-card--press', className)} onClick={onClick}>
      {body}
    </button>
  ) : (
    <div className={cx('cl-card', 'cl-stack', className)}>{body}</div>
  );
}

export function ContainerRow({
  name,
  image,
  node,
  ports,
  status,
  compact,
  selected,
  onStart,
  onRestart,
  onLogs,
  onOpen,
  extra,
  className,
}: {
  name: string;
  image?: string | null;
  node?: string;
  ports?: string;
  status: Status;
  compact?: boolean;
  selected?: boolean;
  onStart?: () => void;
  onRestart?: () => void;
  onLogs?: () => void;
  /** Makes the name a button, for rows that lead to the container's page. */
  onOpen?: () => void;
  /** Extra content under the meta line (a step bar, a reason). */
  extra?: ReactNode;
  className?: string;
}) {
  return (
    <div className={cx('cl-crow', selected && 'cl-crow--selected', className)}>
      <Icon name="container" size={20} className="cl-crow__icon" />
      <div className="cl-crow__main">
        {onOpen ? (
          <button type="button" className="cl-crow__name cl-crow__open" onClick={onOpen}>
            {name}
          </button>
        ) : (
          <span className="cl-crow__name">{name}</span>
        )}
        {image && <span className="cl-code-sm cl-muted cl-crow__image">{image}</span>}
        <span className="cl-sm cl-muted">{[node, ports].filter(Boolean).join(' · ')}</span>
        {extra}
      </div>
      <StatusBadge status={status} compact={compact} />
      <div className="cl-crow__actions">
        {status === 'stopped' ? (
          <Button iconOnly icon="play" label={`Start ${name}`} variant="ghost" onClick={onStart} />
        ) : (
          <Button iconOnly icon="restart" label={`Restart ${name}`} variant="ghost" onClick={onRestart} />
        )}
        <Button iconOnly icon="terminal" label={`Logs for ${name}`} variant="ghost" onClick={onLogs} />
      </div>
    </div>
  );
}
