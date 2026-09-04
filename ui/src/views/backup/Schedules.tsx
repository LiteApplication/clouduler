// Per-drive backup schedules: which destination a drive backs up to, how
// often, and how much history restic keeps. A drive can have several of
// these, one per destination - see DriveBackupTarget in api/types.ts.

import { useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { api, ApiError } from '../../api/client';
import type { BackupDestinationSummary, DriveBackupTarget, DriveStatus } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { useClusterState } from '../../state/store';
import { Icon } from '../../components/Icon';
import { Badge } from '../../components/Badge';
import { CheckboxField, NumberField, SelectField, TimeOfDayField } from '../../components/FormFields';

interface Draft {
  id: string;
  drive: string;
  destination_id: string;
  enabled: boolean;
  frequency_hours: number;
  time_of_day: string;
  keep_last: number;
  keep_within_days: number;
  stop_containers: 'default' | 'yes' | 'no';
}

const emptyDraft = (drive: string, destination_id: string): Draft => ({
  id: '',
  drive,
  destination_id,
  enabled: true,
  frequency_hours: 24,
  time_of_day: '',
  keep_last: 7,
  keep_within_days: 0,
  stop_containers: 'default',
});

export function Schedules() {
  const { isAdmin } = useAuth();
  const cluster = useClusterState();
  const [drives, setDrives] = useState<DriveStatus[]>([]);
  const [destinations, setDestinations] = useState<BackupDestinationSummary[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [showForm, setShowForm] = useState(false);
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState<Draft>(emptyDraft('', ''));
  const [running, setRunning] = useState<string | null>(null);

  const load = () => {
    Promise.all([api.listDrives(), api.listBackupDestinations()])
      .then(([d, dest]) => {
        setDrives(d);
        setDestinations(dest);
      })
      .catch((e) => setError(e instanceof ApiError ? e.message : 'failed to load backup schedules'));
  };

  useEffect(load, []);

  const targets = cluster ? Object.values(cluster.restic_targets) : null;

  const startCreate = () => {
    setEditing(null);
    setDraft(emptyDraft(drives[0]?.name ?? '', destinations[0]?.id ?? ''));
    setShowForm(true);
  };

  const startEdit = (t: DriveBackupTarget) => {
    setEditing(t.id);
    setDraft({
      id: t.id,
      drive: t.drive,
      destination_id: t.destination_id,
      enabled: t.enabled,
      frequency_hours: Math.max(1, Math.round(t.frequency_secs / 3600)),
      time_of_day: t.time_of_day ?? '',
      keep_last: t.keep_last ?? 0,
      keep_within_days: t.keep_within_secs ? Math.round(t.keep_within_secs / 86400) : 0,
      stop_containers: t.stop_containers === null || t.stop_containers === undefined ? 'default' : t.stop_containers ? 'yes' : 'no',
    });
    setShowForm(true);
  };

  const save = async () => {
    setError(null);
    const id = editing ?? `${draft.drive}-${draft.destination_id}`;
    try {
      await api.putBackupTarget({
        id,
        drive: draft.drive,
        destination_id: draft.destination_id,
        enabled: draft.enabled,
        frequency_secs: Math.max(1, draft.frequency_hours) * 3600,
        time_of_day: draft.time_of_day || null,
        keep_last: draft.keep_last > 0 ? draft.keep_last : null,
        keep_within_secs: draft.keep_within_days > 0 ? draft.keep_within_days * 86400 : null,
        stop_containers: draft.stop_containers === 'default' ? null : draft.stop_containers === 'yes',
      });
      setShowForm(false);
      setEditing(null);
      load();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'failed to save backup schedule');
    }
  };

  const remove = async (id: string) => {
    if (!confirm('Remove this backup schedule?')) return;
    try {
      await api.removeBackupTarget(id);
      load();
    } catch (e) {
      alert(e instanceof ApiError ? e.message : 'failed to remove backup schedule');
    }
  };

  const runNow = async (id: string) => {
    setRunning(id);
    try {
      await api.runBackupTarget(id);
    } catch (e) {
      alert(e instanceof ApiError ? e.message : 'failed to start backup run');
    } finally {
      setRunning(null);
      load();
    }
  };

  const destinationName = (id: string) => destinations.find((d) => d.id === id)?.name ?? id;

  if (!targets) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading backup schedules...
      </p>
    );
  }

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="clock" />
          <div>
            <h1>Backup schedules</h1>
            <p className="page-sub">When each drive backs up, to which destination, and how much history restic keeps.</p>
          </div>
        </div>
      </div>

      <div className="section">
        <div className="section-head">
          <h2>
            {targets.length} schedule{targets.length === 1 ? '' : 's'}
          </h2>
          {isAdmin && (
            <button
              onClick={showForm ? () => setShowForm(false) : startCreate}
              disabled={!showForm && (drives.length === 0 || destinations.length === 0)}
            >
              <Icon name={showForm ? 'close' : 'plus'} />
              {showForm ? 'Cancel' : 'New schedule'}
            </button>
          )}
        </div>
        {destinations.length === 0 && (
          <div className="banner banner-warn">
            <Icon name="alert" /> No backup destinations yet - add one under{' '}
            <Link to="/backup/destinations">Destinations</Link> first.
          </div>
        )}
        {error && (
          <div className="banner banner-warn">
            <Icon name="alert" /> {error}
          </div>
        )}

        {showForm && (
          <div className="card section">
            <SelectField
              id="sched-drive"
              label="Drive"
              value={draft.drive}
              onChange={(drive) => setDraft({ ...draft, drive })}
              options={drives.map((d) => ({ value: d.name, label: d.name }))}
            />
            <SelectField
              id="sched-destination"
              label="Destination"
              value={draft.destination_id}
              onChange={(destination_id) => setDraft({ ...draft, destination_id })}
              options={destinations.map((d) => ({ value: d.id, label: d.name }))}
            />
            <NumberField
              id="sched-frequency"
              label="Frequency (hours)"
              value={draft.frequency_hours}
              onChange={(frequency_hours) => setDraft({ ...draft, frequency_hours })}
              hint="Minimum time between runs."
            />
            <TimeOfDayField
              id="sched-time"
              label="Time of day (UTC)"
              value={draft.time_of_day}
              onChange={(time_of_day) => setDraft({ ...draft, time_of_day })}
              hint="Optional. Leave blank to run as soon as the frequency has elapsed."
            />
            <NumberField
              id="sched-keep-last"
              label="Keep last N snapshots"
              value={draft.keep_last}
              onChange={(keep_last) => setDraft({ ...draft, keep_last })}
              hint="0 means no limit on count."
            />
            <NumberField
              id="sched-keep-within"
              label="Keep snapshots within (days)"
              value={draft.keep_within_days}
              onChange={(keep_within_days) => setDraft({ ...draft, keep_within_days })}
              hint="0 means no age-based limit."
            />
            <SelectField
              id="sched-stop"
              label="Stop containers before backup"
              value={draft.stop_containers}
              onChange={(stop_containers) => setDraft({ ...draft, stop_containers })}
              options={[
                { value: 'default', label: "Use the stack's x-clouduler.backup.stop_before (default: yes)" },
                { value: 'yes', label: 'Always stop' },
                { value: 'no', label: 'Never stop' },
              ]}
            />
            <CheckboxField id="sched-enabled" label="Enabled" value={draft.enabled} onChange={(enabled) => setDraft({ ...draft, enabled })} />
            <button onClick={save}>
              <Icon name="check" /> {editing ? 'Save' : 'Create'}
            </button>
          </div>
        )}

        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Drive</th>
                <th>Destination</th>
                <th>Frequency</th>
                <th>Time of day</th>
                <th>Retention</th>
                <th>Last run</th>
                <th>Enabled</th>
                {isAdmin && <th>Actions</th>}
              </tr>
            </thead>
            <tbody>
              {targets.map((t) => (
                <tr key={t.id}>
                  <td>
                    <Link className="row-link" to={`/drives/${encodeURIComponent(t.drive)}`}>
                      {t.drive}
                    </Link>
                  </td>
                  <td>{destinationName(t.destination_id)}</td>
                  <td>every {Math.round(t.frequency_secs / 3600)}h</td>
                  <td>{t.time_of_day ?? 'any'}</td>
                  <td>
                    {t.keep_last ? `last ${t.keep_last}` : ''}
                    {t.keep_last && t.keep_within_secs ? ', ' : ''}
                    {t.keep_within_secs ? `${Math.round(t.keep_within_secs / 86400)}d` : ''}
                    {!t.keep_last && !t.keep_within_secs ? 'forever' : ''}
                  </td>
                  <td>{t.last_run ? new Date(t.last_run * 1000).toLocaleString() : 'never'}</td>
                  <td>
                    <Badge tone={t.enabled ? 'good' : 'neutral'}>{t.enabled ? 'yes' : 'no'}</Badge>
                  </td>
                  {isAdmin && (
                    <td>
                      <button disabled={running === t.id} onClick={() => runNow(t.id)}>
                        <Icon name="play" /> Run now
                      </button>
                      <button onClick={() => startEdit(t)}>
                        <Icon name="edit" /> Edit
                      </button>
                      <button className="danger" onClick={() => remove(t.id)}>
                        <Icon name="trash" /> Remove
                      </button>
                    </td>
                  )}
                </tr>
              ))}
              {targets.length === 0 && (
                <tr>
                  <td colSpan={8} className="empty-cell">
                    no backup schedules
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
