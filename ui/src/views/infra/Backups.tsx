// The Backups tab of /storage: schedules, recent runs, destinations, restores
// and the snapshots each destination holds.

import { useCallback, useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { api, ApiError } from '../../api/client';
import type { BackupDestinationSummary, BackupRun, DriveBackupTarget, DriveStatus, RestoreRequest, SnapshotInfo } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Banner, Button, Chip, Meter, Panel, StatusBadge, Tag, useConfirm, useToast } from '../../ds';
import { relativeTime } from '../../lib/format';
import { runStatus } from '../../lib/status';
import { useClusterState } from '../../state/store';
import { DestinationSheet, KIND_LABEL, RestoreSheet, ScheduleSheet, summarizeKind, type RestorePreset } from './BackupForms';
import { DataTable } from './table';
import { StorageSection } from './volumes';

function runState(run: BackupRun): 'running' | 'ok' | 'failed' {
  return 'Running' in run.outcome ? 'running' : 'Succeeded' in run.outcome ? 'ok' : 'failed';
}

function restoreView(status: RestoreRequest['status']) {
  if (typeof status === 'object' && 'Failed' in status) return { ...runStatus('failed'), label: `Failed: ${status.Failed.error}` };
  if (status === 'Running') return runStatus('running');
  if (status === 'Succeeded') return runStatus('ok');
  return runStatus('pending');
}

function scopeLabel(scope: RestoreRequest['scope']): string {
  if ('Drive' in scope) return `drive ${scope.Drive.drive}`;
  if ('Volume' in scope) return `volume ${scope.Volume.volume}`;
  return `${scope.Service.stack}/${scope.Service.service}`;
}

export function Backups() {
  const { isAdmin } = useAuth();
  const cluster = useClusterState();
  const confirm = useConfirm();
  const toast = useToast();
  const [drives, setDrives] = useState<DriveStatus[]>([]);
  const [destinations, setDestinations] = useState<BackupDestinationSummary[]>([]);
  const [nodeNames, setNodeNames] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [scheduleSheet, setScheduleSheet] = useState<{ open: boolean; editing: DriveBackupTarget | null }>({ open: false, editing: null });
  const [destSheet, setDestSheet] = useState<{ open: boolean; editing: BackupDestinationSummary | null }>({ open: false, editing: null });
  const [restore, setRestore] = useState<{ open: boolean; preset: RestorePreset }>({ open: false, preset: {} });
  const [snapDest, setSnapDest] = useState('');
  const [snapshots, setSnapshots] = useState<SnapshotInfo[] | null>(null);

  const load = useCallback(() => {
    Promise.all([api.listDrives(), api.listBackupDestinations()])
      .then(([d, dest]) => {
        setDrives(d);
        setDestinations(dest);
        setSnapDest((cur) => cur || dest[0]?.id || '');
      })
      .catch((e) => setError(e instanceof ApiError ? e.message : 'Could not load backups.'));
  }, []);
  useEffect(load, [load]);
  useEffect(() => {
    api.getConfiguration().then((c) => setNodeNames(Object.keys(c.nodes))).catch(() => {});
  }, []);
  useEffect(() => {
    if (!snapDest) return setSnapshots(null);
    setSnapshots(null);
    api.listSnapshots(snapDest).then(setSnapshots).catch(() => setSnapshots([]));
  }, [snapDest]);

  if (!cluster) return <p className="sub">Loading…</p>;

  const targets = Object.values(cluster.restic_targets);
  const runs = Object.values(cluster.restic_runs).sort((a, b) => b.started_at - a.started_at);
  const restores = Object.values(cluster.restore_requests).sort((a, b) => b.requested_at - a.requested_at);
  const destName = (id: string) => destinations.find((d) => d.id === id)?.name ?? cluster.restic_destinations[id]?.name ?? id;
  const failedRuns = runs.filter((r) => 'Failed' in r.outcome).slice(0, 1);
  const activeRun = (targetId: string) => runs.find((r) => r.target_id === targetId && 'Running' in r.outcome);

  const runNow = async (t: DriveBackupTarget) => {
    try {
      await api.runBackupTarget(t.id);
      toast(`Backing up ${t.drive}.`);
    } catch (e) {
      toast(e instanceof ApiError ? e.message : 'Could not start the backup.', { tone: 'danger' });
    }
  };
  const removeTarget = async (t: DriveBackupTarget) => {
    if (!(await confirm({ title: 'Remove this schedule?', body: `${t.drive} to ${destName(t.destination_id)} stops running. Existing snapshots stay.`, confirmLabel: 'Remove schedule', confirmVariant: 'danger' }))) return;
    try {
      await api.removeBackupTarget(t.id);
      load();
    } catch (e) {
      toast(e instanceof ApiError ? e.message : 'Could not remove the schedule.', { tone: 'danger' });
    }
  };
  const removeDest = async (d: BackupDestinationSummary) => {
    if (!(await confirm({ title: `Remove ${d.name}?`, body: 'Schedules that use it stop running. Snapshots already stored are not deleted.', confirmLabel: `Remove ${d.name}`, confirmVariant: 'danger' }))) return;
    try {
      await api.removeBackupDestination(d.id);
      load();
    } catch (e) {
      toast(e instanceof ApiError ? e.message : 'Could not remove the destination.', { tone: 'danger' });
    }
  };

  return (
    <>
      {error && <Banner tone="danger">{error}</Banner>}
      {failedRuns.map((r) => (
        <Banner key={r.id} tone="danger" title={`The last ${r.drive} backup failed`}>
          {'Failed' in r.outcome ? r.outcome.Failed.error : ''}
        </Banner>
      ))}
      {destinations.length === 0 && (
        <Banner tone="warn" title="No destinations yet">Add one under Destinations, then schedule a drive to back up to it.</Banner>
      )}

      <StorageSection
        id="sched-h"
        title="Schedules"
        sub={`${targets.length} schedule${targets.length === 1 ? '' : 's'}`}
        actions={
          isAdmin && (
            <Button variant="primary" icon="plus" disabled={drives.length === 0 || destinations.length === 0} onClick={() => setScheduleSheet({ open: true, editing: null })}>
              New schedule
            </Button>
          )
        }
      >
        <Panel flush>
          <DataTable
            caption="Backup schedules"
            rows={targets.map((t) => ({ ...t, id: t.id }))}
            empty="No schedules yet."
            columns={[
              { key: 'drive', header: 'Drive', render: (t) => <Link to={`/storage/drives/${encodeURIComponent(t.drive)}`}>{t.drive}</Link> },
              { key: 'dest', header: 'Destination', render: (t) => destName(t.destination_id) },
              { key: 'freq', header: 'Every', render: (t) => `${Math.round(t.frequency_secs / 3600)}h${t.time_of_day ? ` at ${t.time_of_day}` : ''}` },
              {
                key: 'keep',
                header: 'Keeps',
                render: (t) => [t.keep_last ? `last ${t.keep_last}` : '', t.keep_within_secs ? `${Math.round(t.keep_within_secs / 86400)}d` : ''].filter(Boolean).join(', ') || 'forever',
              },
              { key: 'last', header: 'Last run', render: (t) => (t.last_run ? relativeTime(t.last_run) : 'never') },
              {
                key: 'state',
                header: 'State',
                render: (t) => {
                  const run = activeRun(t.id);
                  const p = run && 'Running' in run.outcome ? run.outcome.Running.percent : null;
                  return run ? (p != null ? <Meter label="Running" value={Math.round(p * 100)} /> : <StatusBadge status="updating" label="Running" />) : <Tag tone={t.enabled ? 'ok' : undefined}>{t.enabled ? 'enabled' : 'paused'}</Tag>;
                },
              },
              ...(isAdmin
                ? [{
                    key: 'act',
                    header: '',
                    render: (t: DriveBackupTarget) => (
                      <span className="acts">
                        <Button size="sm" icon="play" disabled={!!activeRun(t.id)} label={`Run ${t.drive} now`} onClick={() => void runNow(t)}>Run now</Button>
                        <Button iconOnly size="sm" variant="ghost" icon="edit" label={`Edit ${t.drive} schedule`} onClick={() => setScheduleSheet({ open: true, editing: t })} />
                        <Button iconOnly size="sm" variant="ghost" icon="trash" label={`Remove ${t.drive} schedule`} onClick={() => void removeTarget(t)} />
                      </span>
                    ),
                  }]
                : []),
            ]}
          />
        </Panel>
      </StorageSection>

      <StorageSection id="runs-h" title="Recent runs">
        <Panel flush>
          <DataTable
            caption="Recent backup runs"
            rows={runs.slice(0, 20).map((r) => ({ ...r, id: r.id }))}
            empty="No backup runs yet."
            columns={[
              { key: 'drive', header: 'Drive', render: (r) => r.drive },
              { key: 'dest', header: 'Destination', render: (r) => destName(r.destination_id) },
              { key: 'when', header: 'Started', render: (r) => relativeTime(r.started_at) },
              { key: 'by', header: 'By', render: (r) => (r.triggered_by === 'Schedule' ? 'schedule' : `manual (${r.triggered_by.Manual.by})`) },
              {
                key: 'out',
                header: 'Outcome',
                render: (r) => {
                  const s = runState(r);
                  if ('Running' in r.outcome) {
                    const p = r.outcome.Running.percent;
                    return p != null ? <Meter label="Running" value={Math.round(p * 100)} /> : <StatusBadge {...runStatus('running')} />;
                  }
                  if ('Succeeded' in r.outcome) {
                    const o = r.outcome.Succeeded;
                    return (
                      <span>
                        <StatusBadge {...runStatus(s)} /> <code>{o.snapshot_id.slice(0, 8)}</code> {(o.bytes / 1e6).toFixed(1)} MB{o.warning ? ` · ${o.warning}` : ''}
                      </span>
                    );
                  }
                  return <StatusBadge {...runStatus('failed')} />;
                },
              },
            ]}
          />
        </Panel>
      </StorageSection>

      <StorageSection
        id="dest-h"
        title="Destinations"
        actions={isAdmin && <Button size="sm" icon="plus" onClick={() => setDestSheet({ open: true, editing: null })}>New destination</Button>}
      >
        <Panel flush>
          <DataTable
            caption="Backup destinations"
            rows={destinations.map((d) => ({ ...d, id: d.id }))}
            empty="No destinations yet."
            columns={[
              { key: 'name', header: 'Name', render: (d) => <strong>{d.name}</strong> },
              { key: 'kind', header: 'Kind', render: (d) => KIND_LABEL[d.kind.kind] },
              { key: 'where', header: 'Location', mono: true, render: (d) => summarizeKind(d.kind) },
              { key: 'cred', header: 'Credentials', render: (d) => (d.has_credentials ? 'set' : d.kind.kind === 'nodelocal' ? '-' : 'missing') },
              ...(isAdmin
                ? [{
                    key: 'act',
                    header: '',
                    render: (d: BackupDestinationSummary) => (
                      <span className="acts">
                        <Button iconOnly size="sm" variant="ghost" icon="edit" label={`Edit ${d.name}`} onClick={() => setDestSheet({ open: true, editing: d })} />
                        <Button iconOnly size="sm" variant="ghost" icon="trash" label={`Remove ${d.name}`} onClick={() => void removeDest(d)} />
                      </span>
                    ),
                  }]
                : []),
            ]}
          />
        </Panel>
      </StorageSection>

      <StorageSection
        id="restore-h"
        title="Restores"
        sub="A restore stops containers that use the data, replaces it on every node, then starts them again."
        actions={isAdmin && <Button size="sm" icon="archive" onClick={() => setRestore({ open: true, preset: {} })}>Restore from backup</Button>}
      >
        <Panel flush>
          <DataTable
            caption="Restore requests"
            rows={restores.slice(0, 20).map((r) => ({ ...r, id: r.id }))}
            empty="No restores yet."
            columns={[
              { key: 'scope', header: 'What', render: (r) => scopeLabel(r.scope) },
              { key: 'when', header: 'Requested', render: (r) => relativeTime(r.requested_at) },
              { key: 'by', header: 'By', render: (r) => r.requested_by },
              { key: 'st', header: 'Status', render: (r) => <StatusBadge {...restoreView(r.status)} /> },
            ]}
          />
        </Panel>
      </StorageSection>

      <StorageSection
        id="snap-h"
        title="Snapshots"
        sub={snapDest ? `In ${destName(snapDest)}.` : undefined}
        actions={
          destinations.length > 0 && (
            <div className="chips" role="group" aria-label="Destination">
              {destinations.map((d) => <Chip key={d.id} label={d.name} selected={snapDest === d.id} onClick={() => setSnapDest(d.id)} />)}
            </div>
          )
        }
      >
        <Panel flush>
          {snapshots === null && snapDest && <p className="sub" style={{ padding: 'var(--space-4)' }}>Reading snapshots…</p>}
          {snapshots?.length === 0 && <p className="sub" style={{ padding: 'var(--space-4)' }}>No snapshots at this destination.</p>}
          {snapshots?.slice(0, 30).map((s) => (
            <div className="li" key={s.id}>
              <code>{s.id.slice(0, 8)}</code>
              <span className="sub">{relativeTime(s.time)}</span>
              {isAdmin && (
                <span className="li__side">
                  <Button size="sm" variant="secondary" icon="archive" onClick={() => setRestore({ open: true, preset: { destinationId: snapDest, snapshot: s.id } })}>
                    Restore {s.id.slice(0, 8)}
                  </Button>
                </span>
              )}
            </div>
          ))}
        </Panel>
      </StorageSection>

      <ScheduleSheet {...scheduleSheet} drives={drives} destinations={destinations} onClose={() => setScheduleSheet({ open: false, editing: null })} onSaved={load} />
      <DestinationSheet {...destSheet} nodeNames={nodeNames} onClose={() => setDestSheet({ open: false, editing: null })} onSaved={load} />
      <RestoreSheet {...restore} drives={drives} destinations={destinations} onClose={() => setRestore({ open: false, preset: {} })} />
    </>
  );
}
