// Backup run log and restore request log, both fed by the same cluster
// state stream everything else in the app uses - see BackupRun/RestoreRequest
// in api/types.ts.

import { Link } from 'react-router-dom';
import { useClusterState } from '../../state/store';
import { Icon } from '../../components/Icon';
import { Badge, type BadgeTone } from '../../components/Badge';
import type { BackupRun, RestoreRequest } from '../../api/types';

function runTone(run: BackupRun): BadgeTone {
  if (run.outcome === 'Running') return 'warn';
  if (typeof run.outcome === 'object' && 'Succeeded' in run.outcome) return 'good';
  return 'bad';
}

function runLabel(run: BackupRun): string {
  if (run.outcome === 'Running') return 'running';
  if (typeof run.outcome === 'object' && 'Succeeded' in run.outcome) return 'succeeded';
  return 'failed';
}

function restoreTone(status: RestoreRequest['status']): BadgeTone {
  if (status === 'Running') return 'warn';
  if (status === 'Succeeded') return 'good';
  if (status === 'Pending') return 'neutral';
  return 'bad';
}

function restoreLabel(status: RestoreRequest['status']): string {
  if (typeof status === 'object' && 'Failed' in status) return `failed: ${status.Failed.error}`;
  return status.toLowerCase();
}

function scopeLabel(scope: RestoreRequest['scope']): string {
  if ('Drive' in scope) return `drive ${scope.Drive.drive}`;
  if ('Volume' in scope) return `volume ${scope.Volume.volume}`;
  return `${scope.Service.stack}/${scope.Service.service}`;
}

function fmt(secs?: number | null): string {
  return secs ? new Date(secs * 1000).toLocaleString() : '-';
}

export function History() {
  const cluster = useClusterState();

  if (!cluster) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading backup history...
      </p>
    );
  }

  const runs = Object.values(cluster.restic_runs).sort((a, b) => b.started_at - a.started_at);
  const restores = Object.values(cluster.restore_requests).sort((a, b) => b.requested_at - a.requested_at);
  const destName = (id: string) => cluster.restic_destinations[id]?.name ?? id;

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="file" />
          <div>
            <h1>Backup &amp; restore history</h1>
            <p className="page-sub">Every scheduled or manual backup run and restore request, newest first.</p>
          </div>
        </div>
        <Link to="/backup/restore">
          <button>
            <Icon name="restart" /> Restore something
          </button>
        </Link>
      </div>

      <div className="section">
        <div className="section-head">
          <h2>Backup runs</h2>
        </div>
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Drive</th>
                <th>Destination</th>
                <th>Started</th>
                <th>Finished</th>
                <th>Triggered by</th>
                <th>Outcome</th>
              </tr>
            </thead>
            <tbody>
              {runs.map((r) => (
                <tr key={r.id}>
                  <td>
                    <Link className="row-link" to={`/drives/${encodeURIComponent(r.drive)}`}>
                      {r.drive}
                    </Link>
                  </td>
                  <td>{destName(r.destination_id)}</td>
                  <td>{fmt(r.started_at)}</td>
                  <td>{fmt(r.finished_at)}</td>
                  <td>{r.triggered_by === 'Schedule' ? 'schedule' : `manual (${r.triggered_by.Manual.by})`}</td>
                  <td>
                    <Badge
                      tone={runTone(r)}
                      title={typeof r.outcome === 'object' && 'Failed' in r.outcome ? r.outcome.Failed.error : undefined}
                    >
                      {runLabel(r)}
                    </Badge>
                    {typeof r.outcome === 'object' && 'Succeeded' in r.outcome && (
                      <span className="mono" style={{ fontSize: 12, marginLeft: 6 }}>
                        {r.outcome.Succeeded.snapshot_id.slice(0, 8)} · {(r.outcome.Succeeded.bytes / 1e6).toFixed(1)} MB
                      </span>
                    )}
                  </td>
                </tr>
              ))}
              {runs.length === 0 && (
                <tr>
                  <td colSpan={6} className="empty-cell">
                    no backup runs yet
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>

      <div className="section">
        <div className="section-head">
          <h2>Restore requests</h2>
        </div>
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Scope</th>
                <th>Requested</th>
                <th>Finished</th>
                <th>Requested by</th>
                <th>Status</th>
              </tr>
            </thead>
            <tbody>
              {restores.map((r) => (
                <tr key={r.id}>
                  <td>{scopeLabel(r.scope)}</td>
                  <td>{fmt(r.requested_at)}</td>
                  <td>{fmt(r.finished_at)}</td>
                  <td>{r.requested_by}</td>
                  <td>
                    <Badge tone={restoreTone(r.status)}>{restoreLabel(r.status)}</Badge>
                  </td>
                </tr>
              ))}
              {restores.length === 0 && (
                <tr>
                  <td colSpan={5} className="empty-cell">
                    no restore requests yet
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
