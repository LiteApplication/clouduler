import { useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import type { MigrationAction, MigrationProblem as MigrationProblemType } from '../../api/types';
import { api } from '../../api/client';
import { ApiError } from '../../api/client';
import { Button, Icon, useConfirm, useToast } from '../../ds';
import { cx } from '../../ds/cx';
import './MigrationProblem.css';

export interface MigrationProblemProps {
  instanceId: string;
  problem: MigrationProblemType;
  isAdmin: boolean;
  onOpenMove?: () => void;
  onActionComplete?: () => void;
  className?: string;
}

export function MigrationProblem({
  instanceId,
  problem,
  isAdmin,
  onOpenMove,
  onActionComplete,
  className,
}: MigrationProblemProps) {
  const navigate = useNavigate();
  const confirm = useConfirm();
  const toast = useToast();
  const [busyAction, setBusyAction] = useState<string | null>(null);
  const [detailsOpen, setDetailsOpen] = useState(false);
  const [filesOpen, setFilesOpen] = useState(false);

  const hasFiles =
    (problem.added && problem.added.length > 0) ||
    (problem.changed && problem.changed.length > 0) ||
    (problem.removed && problem.removed.length > 0);

  const totalFiles =
    problem.files_total ||
    (problem.added?.length ?? 0) + (problem.changed?.length ?? 0) + (problem.removed?.length ?? 0);

  const handleAction = async (action: MigrationAction) => {
    if (action.id === 'move_elsewhere') {
      if (onOpenMove) {
        onOpenMove();
      } else {
        navigate(`/instances/${encodeURIComponent(instanceId)}`);
      }
      return;
    }

    if (action.id === 'view_logs') {
      navigate(`/instances/${encodeURIComponent(instanceId)}#logs`);
      return;
    }

    if (action.id === 'view_volume') {
      navigate('/storage/volumes');
      return;
    }

    if (action.danger || action.confirm) {
      const ok = await confirm({
        title: action.label,
        body: action.confirm ?? action.description,
        confirmLabel: action.label,
        confirmVariant: action.danger ? 'danger' : 'primary',
      });
      if (!ok) return;
    }

    setBusyAction(action.id);
    try {
      if (action.id === 'retry') {
        await api.retryMigration(instanceId);
        toast(`Retrying move to ${problem.to}`);
      } else if (action.id === 'dismiss') {
        await api.dismissMigration(instanceId);
        toast(`Dismissed move to ${problem.to}`);
      } else if (action.id === 'restart') {
        await api.restartInstance(instanceId);
        toast(`Restarting ${instanceId}`);
      }
      onActionComplete?.();
    } catch (e) {
      toast(`Action failed: ${action.label}`, {
        tone: 'danger',
        body: e instanceof ApiError ? e.message : String(e),
      });
    } finally {
      setBusyAction(null);
    }
  };

  return (
    <div
      className={cx('cl-mig-problem', className)}
      role="alert"
    >
      <div className="cl-mig-problem__header">
        <Icon name="x" size={20} className="cl-mig-problem__icon" />
        <div className="cl-mig-problem__title-wrap">
          <strong className="cl-mig-problem__title">{problem.title}</strong>
          <p className="cl-mig-problem__detail">{problem.detail}</p>
        </div>
      </div>

      {problem.co_writers && problem.co_writers.length > 0 && (
        <div className="cl-mig-problem__cowriters">
          <span className="cl-mig-problem__subhead">Also mounts this volume:</span>
          <div className="cl-mig-problem__cowriter-chips">
            {problem.co_writers.map((cw) => (
              <Link
                key={cw.instance}
                to={`/instances/${encodeURIComponent(cw.instance)}`}
                className="cl-mig-problem__cowriter-link"
              >
                <Icon name="container" size={14} />
                <span>{cw.instance}</span>
                <span className="cl-mig-problem__node-tag">({cw.node})</span>
                {cw.running ? (
                  <span className="cl-mig-problem__state-running">running</span>
                ) : (
                  <span className="cl-mig-problem__state-stopped">stopped</span>
                )}
              </Link>
            ))}
          </div>
        </div>
      )}

      {hasFiles && (
        <div className="cl-mig-problem__files">
          <button
            type="button"
            className="cl-mig-problem__toggle"
            onClick={() => setFilesOpen(!filesOpen)}
            aria-expanded={filesOpen}
          >
            <Icon name="chevron" size={14} className={cx('cl-mig-problem__chev', filesOpen && 'cl-mig-problem__chev--open')} />
            <span>
              {filesOpen
                ? 'Hide differing files'
                : `Show differing files (${totalFiles} ${totalFiles === 1 ? 'file' : 'files'})`}
            </span>
          </button>
          {filesOpen && (
            <div className="cl-mig-problem__file-lists">
              {problem.changed && problem.changed.length > 0 && (
                <div className="cl-mig-problem__file-group">
                  <span className="cl-mig-problem__file-label">Changed:</span>
                  <ul className="cl-mig-problem__file-ul">
                    {problem.changed.map((f) => (
                      <li key={f} className="cl-mig-problem__file-item">
                        {f}
                      </li>
                    ))}
                  </ul>
                </div>
              )}
              {problem.added && problem.added.length > 0 && (
                <div className="cl-mig-problem__file-group">
                  <span className="cl-mig-problem__file-label">Added:</span>
                  <ul className="cl-mig-problem__file-ul">
                    {problem.added.map((f) => (
                      <li key={f} className="cl-mig-problem__file-item">
                        {f}
                      </li>
                    ))}
                  </ul>
                </div>
              )}
              {problem.removed && problem.removed.length > 0 && (
                <div className="cl-mig-problem__file-group">
                  <span className="cl-mig-problem__file-label">Removed:</span>
                  <ul className="cl-mig-problem__file-ul">
                    {problem.removed.map((f) => (
                      <li key={f} className="cl-mig-problem__file-item">
                        {f}
                      </li>
                    ))}
                  </ul>
                </div>
              )}
              {problem.files_total != null &&
                problem.files_total >
                  (problem.added?.length ?? 0) +
                    (problem.changed?.length ?? 0) +
                    (problem.removed?.length ?? 0) && (
                <p className="cl-mig-problem__files-more">
                  ...and {problem.files_total - ((problem.added?.length ?? 0) + (problem.changed?.length ?? 0) + (problem.removed?.length ?? 0))} more files
                </p>
              )}
            </div>
          )}
        </div>
      )}

      {isAdmin && problem.actions && problem.actions.length > 0 && (
        <div className="cl-mig-problem__actions">
          {problem.actions.map((act) => (
            <Button
              key={act.id}
              size="sm"
              variant={act.primary ? 'primary' : act.danger ? 'danger' : 'secondary'}
              loading={busyAction === act.id}
              onClick={() => handleAction(act)}
              title={act.description}
            >
              {act.label}
            </Button>
          ))}
        </div>
      )}

      {problem.raw && (
        <div className="cl-mig-problem__technical">
          <button
            type="button"
            className="cl-mig-problem__toggle"
            onClick={() => setDetailsOpen(!detailsOpen)}
            aria-expanded={detailsOpen}
          >
            <Icon name="chevron" size={14} className={cx('cl-mig-problem__chev', detailsOpen && 'cl-mig-problem__chev--open')} />
            <span>Technical details</span>
          </button>
          {detailsOpen && (
            <pre className="cl-mig-problem__raw">{problem.raw}</pre>
          )}
        </div>
      )}
    </div>
  );
}
