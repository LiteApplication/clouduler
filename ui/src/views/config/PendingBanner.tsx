// Shown wherever a config edit can be applied (the Configuration shell and a
// node's own detail page), so the operator sees the same auto-rollback
// countdown and confirm/roll-back actions no matter where the edit was made.

import type { PendingConfig } from '../../api/types';
import { Icon } from '../../components/Icon';

export function PendingBanner({
  pending,
  onConfirm,
  onRollback,
}: {
  pending: PendingConfig;
  onConfirm: () => void;
  onRollback: () => void;
}) {
  return (
    <div className="banner banner-warn pending-banner" role="alert">
      <div className="flex-row">
        <Icon name="clock" />
        <strong>A config change is pending confirmation.</strong>
      </div>
      <span className="countdown">{pending.seconds_left ?? '?'}s</span>
      <span>
        If you do not confirm before the timer runs out, the previous config is restored automatically. This
        protects you from editing away the network you are connected over.
      </span>
      <div className="btn-row">
        <button className="primary" onClick={onConfirm}>
          <Icon name="check" /> Confirm new config
        </button>
        <button className="danger" onClick={onRollback}>
          <Icon name="restart" /> Roll back now
        </button>
      </div>
    </div>
  );
}
