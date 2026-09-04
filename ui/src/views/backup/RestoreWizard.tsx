// Restores a whole drive, a single volume, or every volume a service uses,
// from a restic snapshot. Always destructive - see backup_restore.rs on the
// backend for why containers using the target are stopped first, no
// exceptions. Submits a RestoreRequest; whichever node holds the drive
// claims and executes it (see backup_restore::start_restore_watcher).

import { useEffect, useMemo, useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import { api, ApiError } from '../../api/client';
import type { BackupDestinationSummary, DriveStatus, InstanceState, SnapshotInfo } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Icon } from '../../components/Icon';
import { CheckboxField, SelectField, TextField } from '../../components/FormFields';

type Scope = 'drive' | 'volume' | 'service';

function parseInstanceId(id: string): { stack: string; service: string } | null {
  const parts = id.split('.');
  if (parts.length < 3) return null;
  return { stack: parts.slice(0, parts.length - 2).join('.'), service: parts[parts.length - 2] };
}

export function RestoreWizard() {
  const { isAdmin } = useAuth();
  const [params] = useSearchParams();

  const [scope, setScope] = useState<Scope>((params.get('scope') as Scope) ?? 'drive');
  const [drive, setDrive] = useState(params.get('drive') ?? '');
  const [volume, setVolume] = useState(params.get('volume') ?? '');
  const [stack, setStack] = useState(params.get('stack') ?? '');
  const [service, setService] = useState(params.get('service') ?? '');
  const [destinationId, setDestinationId] = useState('');
  const [snapshot, setSnapshot] = useState('');
  const [acknowledged, setAcknowledged] = useState(false);

  const [drives, setDrives] = useState<DriveStatus[]>([]);
  const [destinations, setDestinations] = useState<BackupDestinationSummary[]>([]);
  const [instances, setInstances] = useState<InstanceState[]>([]);
  const [snapshots, setSnapshots] = useState<SnapshotInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [submitted, setSubmitted] = useState<string | null>(null);

  useEffect(() => {
    api.listDrives().then(setDrives).catch(() => {});
    api.listBackupDestinations().then(setDestinations).catch(() => {});
    api.listInstances().then(setInstances).catch(() => {});
  }, []);

  useEffect(() => {
    if (!destinationId) {
      setSnapshots(null);
      return;
    }
    api
      .listSnapshots(destinationId)
      .then(setSnapshots)
      .catch(() => setSnapshots([]));
  }, [destinationId]);

  const stackNames = useMemo(() => [...new Set(instances.map((i) => parseInstanceId(i.id)?.stack).filter((s): s is string => !!s))], [instances]);
  const serviceNames = useMemo(
    () => [...new Set(instances.filter((i) => parseInstanceId(i.id)?.stack === stack).map((i) => parseInstanceId(i.id)!.service))],
    [instances, stack],
  );
  const canAdvanceFrom1 = scope === 'drive' ? !!drive : scope === 'volume' ? !!drive && !!volume : !!stack;

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      let id: string;
      if (scope === 'drive') {
        id = await api.restoreDrive({ drive, destination_id: destinationId, snapshot: snapshot || null });
      } else if (scope === 'volume') {
        id = await api.restoreVolume({ drive, volume: `${drive}/${volume}`, destination_id: destinationId, snapshot: snapshot || null });
      } else {
        id = await api.restoreService({
          stack,
          service,
          destination_id: destinationId || null,
          snapshot: snapshot || null,
        });
      }
      setSubmitted(id);
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'failed to submit restore request');
    } finally {
      setBusy(false);
    }
  };

  if (!isAdmin) {
    return (
      <p className="field-error">
        <Icon name="alert-circle" /> Only admins can restore data.
      </p>
    );
  }

  if (submitted) {
    return (
      <div>
        <div className="page-header">
          <div className="page-title">
            <Icon name="restart" />
            <div>
              <h1>Restore submitted</h1>
            </div>
          </div>
        </div>
        <div className="banner banner-good">
          <Icon name="check-circle" /> Restore request <span className="mono">{submitted}</span> submitted. Track it
          on the <a href="/backup/history">History</a> page.
        </div>
      </div>
    );
  }

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="restart" />
          <div>
            <h1>Restore</h1>
            <p className="page-sub">Bring a drive, volume, or service's data back from a restic snapshot.</p>
          </div>
        </div>
      </div>

      {error && (
        <div className="banner banner-warn">
          <Icon name="alert" /> {error}
        </div>
      )}

      <div className="card section">
        <div className="section-head">
          <h2>1. What to restore</h2>
        </div>
        <div className="field">
          <label>Scope</label>
          <div className="btn-row" role="group" aria-label="Restore scope">
            {(['drive', 'volume', 'service'] as Scope[]).map((s) => (
              <button
                key={s}
                type="button"
                className={scope === s ? 'primary' : ''}
                aria-pressed={scope === s}
                onClick={() => setScope(s)}
              >
                {s === 'drive' ? 'Whole drive' : s === 'volume' ? 'Single volume' : "A service's volumes"}
              </button>
            ))}
          </div>
        </div>

        {(scope === 'drive' || scope === 'volume') && (
          <SelectField
            id="restore-drive"
            label="Drive"
            value={drive}
            onChange={setDrive}
            options={drives.map((d) => ({ value: d.name, label: d.name }))}
          />
        )}
        {scope === 'volume' && (
          <TextField id="restore-volume" label="Volume" value={volume} onChange={setVolume} hint={`Under drive ${drive || '...'}`} />
        )}
        {scope === 'service' && (
          <>
            <SelectField id="restore-stack" label="Stack" value={stack} onChange={(s) => { setStack(s); setService(''); }} options={stackNames.map((s) => ({ value: s, label: s }))} />
            <SelectField
              id="restore-service"
              label="Service"
              value={service}
              onChange={setService}
              options={serviceNames.map((s) => ({ value: s, label: s }))}
            />
            <p className="field-hint">
              Restores every volume this service uses. If a volume backs up to more than one destination, pick which
              one below - otherwise it's resolved automatically.
            </p>
          </>
        )}
      </div>

      <div className="card section">
        <div className="section-head">
          <h2>2. Source</h2>
        </div>
        <SelectField
          id="restore-destination"
          label={scope === 'service' ? 'Destination (optional)' : 'Destination'}
          value={destinationId}
          onChange={setDestinationId}
          options={[
            ...(scope === 'service' ? [{ value: '', label: 'Resolve automatically' }] : []),
            ...destinations.map((d) => ({ value: d.id, label: d.name })),
          ]}
        />
        <SelectField
          id="restore-snapshot"
          label="Snapshot"
          value={snapshot}
          onChange={setSnapshot}
          disabled={!destinationId}
          options={[
            { value: '', label: 'Latest' },
            ...(snapshots ?? []).map((s) => ({ value: s.id, label: `${new Date(s.time).toLocaleString()} · ${s.id.slice(0, 8)}` })),
          ]}
        />
      </div>

      <div className="card section">
        <div className="section-head">
          <h2>3. Confirm</h2>
        </div>
        <div className="banner banner-bad">
          <Icon name="alert" />
          <span>
            This restores {scope === 'drive' ? `drive "${drive}"` : scope === 'volume' ? `volume "${drive}/${volume}"` : `${stack}/${service}'s volumes`}{' '}
            from {snapshot ? `snapshot ${snapshot.slice(0, 8)}` : 'the latest snapshot'}, overwriting the data currently
            there. Containers using it are stopped first and restarted once the restore finishes.
          </span>
        </div>
        <CheckboxField
          id="restore-ack"
          label="I understand this overwrites current data"
          value={acknowledged}
          onChange={setAcknowledged}
        />
        <button
          className="danger"
          disabled={busy || !acknowledged || !canAdvanceFrom1 || (scope !== 'service' && !destinationId)}
          onClick={submit}
        >
          <Icon name="restart" /> {busy ? 'Submitting...' : 'Start restore'}
        </button>
      </div>
    </div>
  );
}
