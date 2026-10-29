import { Icon, type IconName } from './Icon';
import { cx } from './cx';

export type Status = 'running' | 'healthy' | 'degraded' | 'failed' | 'unreachable' | 'stopped' | 'updating';

const STATUS: Record<Status, { icon: IconName; label: string; tone: 'ok' | 'warn' | 'danger' | 'muted' | 'info' }> = {
  running: { icon: 'check-circle', label: 'Running', tone: 'ok' },
  healthy: { icon: 'check-circle', label: 'Healthy', tone: 'ok' },
  degraded: { icon: 'alert', label: 'Degraded', tone: 'warn' },
  failed: { icon: 'x', label: 'Failed', tone: 'danger' },
  unreachable: { icon: 'x', label: 'Unreachable', tone: 'danger' },
  stopped: { icon: 'pause', label: 'Stopped', tone: 'muted' },
  updating: { icon: 'sync', label: 'Updating', tone: 'info' },
};

export function StatusBadge({
  status,
  label,
  compact,
  className,
}: {
  status: Status;
  label?: string;
  /** Icon only, with the word kept for assistive tech and the tooltip. */
  compact?: boolean;
  className?: string;
}) {
  const s = STATUS[status] ?? STATUS.stopped;
  const text = label ?? s.label;
  return (
    <span
      className={cx('cl-badge', `cl-badge--${s.tone}`, compact && 'cl-badge--compact', className)}
      title={compact ? text : undefined}
    >
      <Icon name={s.icon} size={14} className={status === 'updating' ? 'cl-spin' : undefined} />
      {compact ? <span className="cl-sr">{text}</span> : <span>{text}</span>}
    </span>
  );
}
