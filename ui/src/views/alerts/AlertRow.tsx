import { useState } from 'react';
import { Link } from 'react-router-dom';
import type { AlertView } from '../../api/types';
import { runAlertAction, ApiError } from '../../api/client';
import { Button, Icon, StatusBadge, Tag } from '../../ds';
import { useAlerts } from '../../state/alertsStore';
import { filedAt, sentSummary } from '../../lib/alerts';
import { relativeTime } from '../../lib/format';

function SeverityBadge({ a }: { a: AlertView }) {
  if (a.severity === 'critical') return <StatusBadge status="failed" label="Critical" />;
  if (a.severity === 'warning') return <StatusBadge status="degraded" label="Warning" />;
  return (
    <Tag>
      <Icon name="info" size={14} />
      Info
    </Tag>
  );
}

export function AlertRow({ alert: a, isNew, active }: { alert: AlertView; isNew: boolean; active?: boolean }) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const { refresh } = useAlerts();
  const sent = sentSummary(a);
  const primary = a.links.find((l) => !l.method);
  const actions = a.links.filter((l) => l.method);
  const act = async (method: NonNullable<(typeof actions)[number]['method']>, path: string) => {
    setBusy(true);
    setActionError(null);
    try {
      await runAlertAction(method, path);
      refresh();
    } catch (e) {
      setActionError(e instanceof ApiError ? e.message : 'The action failed.');
    } finally {
      setBusy(false);
    }
  };
  const resolved = a.state === 'resolved';
  const when = active ? a.opened_at : filedAt(a);
  return (
    <div className={`al${isNew ? ' al--new' : ''}${a.state === 'pending' ? ' al--pending' : ''}`}>
      <div className="al__sev">
        <SeverityBadge a={a} />
        {resolved && (
          <Tag tone="ok">
            <Icon name="check" size={14} />
            Resolved
          </Tag>
        )}
        {a.state === 'pending' && <Tag tone="warn">Waiting</Tag>}
        {a.escalated_from && !resolved && <Tag tone="danger">Was {a.escalated_from}</Tag>}
        {isNew && <Tag tone="brand">New</Tag>}
      </div>
      <div className="al__main">
        <span className="al__t">{a.title}</span>
        {a.detail && <span className="sub">{a.detail}</span>}
        <span className="al__key">
          {a.event}
          {a.subject ? ` · ${a.subject}` : ''}
          {a.count > 1 ? ` · ${a.count} times` : ''}
        </span>
        <div className="al__to">
          <span className="sub">
            {a.state === 'pending' ? 'Will be sent if it lasts past the debounce' : sent.text}
          </span>
          {sent.failed.map((f) => (
            <Tag key={f.destination} tone="danger">
              <Icon name="x" size={14} />
              {f.destination} {f.error ? `failed: ${f.error}` : 'failed'}
            </Tag>
          ))}
        </div>
        {a.excerpt.length > 0 && (
          <>
            <button type="button" className="al__more" onClick={() => setOpen(!open)} aria-expanded={open}>
              {open ? 'Hide log lines' : `Show ${a.excerpt.length} log line${a.excerpt.length === 1 ? '' : 's'}`}
            </button>
            {open && <pre className="codeblock al__excerpt">{a.excerpt.join('\n')}</pre>}
          </>
        )}
      </div>
      <div className="al__side">
        <span className="al__when" title={new Date(when * 1000).toLocaleString()}>
          {relativeTime(when)}
        </span>
        {!resolved &&
          actions.map((l) => (
            <Button key={l.path} variant="primary" loading={busy} onClick={() => void act(l.method!, l.path)}>
              {l.label}
            </Button>
          ))}
        {actionError && <span className="sub">{actionError}</span>}
        {primary && (
          <Link className="cl-btn cl-btn--secondary cl-btn--md" to={primary.path}>
            <span>{primary.label}</span>
          </Link>
        )}
      </div>
    </div>
  );
}
