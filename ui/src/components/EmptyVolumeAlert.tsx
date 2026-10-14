import { useEffect, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { api, ApiError } from '../api/client';
import type { VolumeInspection, VolumeState } from '../api/types';
import { useAuth } from '../auth/AuthContext';
import { Icon } from './Icon';

function age(unixSecs: number): string {
  const secs = Math.max(0, Math.floor(Date.now() / 1000) - unixSecs);
  const days = Math.floor(secs / 86400);
  if (days >= 1) return `${days} day${days === 1 ? '' : 's'}`;
  const hours = Math.floor(secs / 3600);
  if (hours >= 1) return `${hours} hour${hours === 1 ? '' : 's'}`;
  return `${Math.max(1, Math.floor(secs / 60))} min`;
}

function One({ volume }: { volume: VolumeState }) {
  const { isAdmin } = useAuth();
  const navigate = useNavigate();
  const [drive, ...rest] = volume.id.split('/');
  const name = rest.join('/');
  const [inspection, setInspection] = useState<VolumeInspection | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let cancelled = false;
    api
      .inspectVolume(drive, name)
      .then((i) => !cancelled && setInspection(i))
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [drive, name]);

  const remove = async () => {
    if (
      !confirm(
        `Delete ${volume.id} on every node of ${drive}? It holds no files, so nothing is lost, but it cannot be undone.`,
      )
    )
      return;
    setBusy(true);
    setError(null);
    try {
      await api.deleteVolume(drive, name);
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'delete failed');
    } finally {
      setBusy(false);
    }
  };

  const restore = `/backup/restore?scope=volume&drive=${encodeURIComponent(drive)}&volume=${encodeURIComponent(name)}`;

  return (
    <div className="banner banner-bad" role="alert" style={{ flexWrap: 'wrap' }}>
      <Icon name="alert" />
      <div style={{ flex: 1, minWidth: 240 }}>
        <div>
          Volume <code>{volume.id}</code> is empty
        </div>
        <div style={{ fontWeight: 400 }}>
          {inspection && inspection.newest_file_at !== null
            ? `Its newest file is ${age(inspection.newest_file_at)} old. `
            : 'It has no files on its source node. '}
          If a database was running on it, it may have started a fresh empty one. Nothing is changed until you choose.
        </div>
        {error && <div className="field-error">{error}</div>}
      </div>
      <button className="primary" onClick={() => navigate(restore)}>
        Restore from backup
      </button>
      {isAdmin && (
        <button className="danger" onClick={remove} disabled={busy}>
          Delete everywhere
        </button>
      )}
    </div>
  );
}

/** Shown wherever volumes are listed. `drive` narrows it to one drive. */
export function EmptyVolumeAlerts({ volumes, drive }: { volumes: VolumeState[]; drive?: string }) {
  const empty = volumes.filter((v) => v.looks_empty && (!drive || v.id.startsWith(`${drive}/`)));
  if (empty.length === 0) return null;
  return (
    <>
      {empty.map((v) => (
        <One key={v.id} volume={v} />
      ))}
    </>
  );
}
