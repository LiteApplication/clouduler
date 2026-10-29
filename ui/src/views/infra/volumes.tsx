// Volume handling shared by Storage and the drive page: the state banners, the
// row actions (sync, compare, freeze) and the per-node comparison sheet.

import { useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { api, ApiError } from '../../api/client';
import type { VolumeInspection, VolumeState } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Banner, BottomSheet, Button, StatusBadge, Tag, useConfirm, useToast } from '../../ds';
import { formatBytes, relativeTime } from '../../lib/format';
import { volumeStatus } from '../../lib/status';
import { DataTable } from './table';

export function splitId(id: string): [string, string] {
  const [drive, ...rest] = id.split('/');
  return [drive, rest.join('/')];
}

/** One banner per volume that needs a human: refused syncs, frozen volumes
 *  and volumes the source found empty. Nothing here changes data on its own. */
export function VolumeBanners({ volumes, onCompare }: { volumes: VolumeState[]; onCompare: (v: VolumeState) => void }) {
  const toast = useToast();
  const { isAdmin } = useAuth();
  const unfreeze = async (v: VolumeState) => {
    const [d, n] = splitId(v.id);
    try {
      await api.unfreezeVolume(d, n);
      toast(`Unfroze ${v.id}.`);
    } catch (e) {
      toast(e instanceof ApiError ? e.message : 'Could not unfreeze.', { tone: 'danger' });
    }
  };
  return (
    <>
      {volumes.filter((v) => v.sync_refused).map((v) => (
        <Banner key={`r-${v.id}`} tone="danger" title={`${v.id} refused to sync`} action={isAdmin ? 'Compare copies' : undefined} onAction={() => onCompare(v)}>
          {v.sync_refused!.from} would have overwritten {v.sync_refused!.to}: {v.sync_refused!.reason}. Nothing was changed.
        </Banner>
      ))}
      {volumes.filter((v) => v.frozen).map((v) => (
        <Banner key={`f-${v.id}`} tone="warn" title={`${v.id} is frozen`} action={isAdmin ? 'Unfreeze' : undefined} onAction={() => void unfreeze(v)}>
          Held on {v.frozen!.node} for hand edits. It does not replicate until you unfreeze it.
        </Banner>
      ))}
      {volumes.filter((v) => v.looks_empty && !v.sync_refused).map((v) => (
        <Banner key={`e-${v.id}`} tone="danger" title={`${v.id} looks empty`} action={isAdmin ? 'Compare copies' : undefined} onAction={() => onCompare(v)}>
          If a database was running on it, it may have started a fresh one. Nothing changes until you choose.
        </Banner>
      ))}
    </>
  );
}

/** The volumes table. `onCompare` opens the comparison sheet for a row. */
export function VolumesTable({ volumes, onCompare, showDrive = true }: { volumes: VolumeState[]; onCompare: (v: VolumeState) => void; showDrive?: boolean }) {
  const { isAdmin } = useAuth();
  const toast = useToast();
  const confirm = useConfirm();

  const sync = async (v: VolumeState) => {
    const [d, n] = splitId(v.id);
    try {
      await api.syncVolume(d, n);
      toast(`Syncing ${v.id}.`);
    } catch (e) {
      toast(e instanceof ApiError ? e.message : 'Could not sync.', { tone: 'danger' });
    }
  };
  const freeze = async (v: VolumeState) => {
    const [d, n] = splitId(v.id);
    const ok = await confirm({
      title: `Freeze ${v.id}?`,
      body: 'Containers using it are stopped so you can edit its files by hand. Unfreeze it to resume replication.',
      confirmLabel: `Freeze ${v.id}`,
      confirmVariant: 'primary',
    });
    if (!ok) return;
    try {
      await api.freezeVolume(d, n);
      toast(`Froze ${v.id}.`);
    } catch (e) {
      toast(e instanceof ApiError ? e.message : 'Could not freeze.', { tone: 'danger' });
    }
  };
  const unfreeze = async (v: VolumeState) => {
    const [d, n] = splitId(v.id);
    try {
      await api.unfreezeVolume(d, n);
      toast(`Unfroze ${v.id}.`);
    } catch (e) {
      toast(e instanceof ApiError ? e.message : 'Could not unfreeze.', { tone: 'danger' });
    }
  };

  return (
    <DataTable
      caption="Volumes"
      rows={volumes.map((v) => ({ ...v, id: v.id }))}
      empty="No volumes yet."
      columns={[
        { key: 'name', header: 'Volume', render: (v) => <strong>{showDrive ? v.id : splitId(v.id)[1]}</strong> },
        { key: 'gen', header: 'Generation', mono: true, render: (v) => v.generation },
        { key: 'primary', header: 'Primary', render: (v) => v.primary },
        {
          key: 'rep',
          header: 'Replicas',
          render: (v) => (
            <span className="row">
              {Object.entries(v.replicas).map(([node, g]) => (
                <Tag key={node} tone={g === v.generation ? 'ok' : 'warn'}>{node} @{g}</Tag>
              ))}
            </span>
          ),
        },
        { key: 'state', header: 'State', render: (v) => { const s = volumeStatus(v); return <StatusBadge status={s.status} label={s.label} />; } },
        ...(isAdmin
          ? [{
              key: 'act',
              header: '',
              render: (v: VolumeState) => (
                <span className="row">
                  <Button size="sm" icon="sync" label={`Sync ${v.id}`} onClick={() => void sync(v)}>Sync</Button>
                  <Button size="sm" variant="ghost" onClick={() => onCompare(v)}>Inspect</Button>
                  {v.frozen ? (
                    <Button size="sm" variant="ghost" onClick={() => void unfreeze(v)}>Unfreeze</Button>
                  ) : (
                    <Button size="sm" variant="ghost" onClick={() => void freeze(v)}>Freeze</Button>
                  )}
                </span>
              ),
            }]
          : []),
      ]}
    />
  );
}

/** What every copy of one volume holds, side by side. The primary is never
 *  overridden from here: restore from a backup or delete the volume. */
export function CompareSheet({ volume, onClose }: { volume: VolumeState | null; onClose: () => void }) {
  const confirm = useConfirm();
  const toast = useToast();
  const [inspections, setInspections] = useState<Record<string, VolumeInspection | string>>({});

  useEffect(() => {
    setInspections({});
    if (!volume) return;
    const [drive, name] = splitId(volume.id);
    let off = false;
    for (const node of Object.keys(volume.replicas)) {
      api
        .inspectVolume(drive, name, node)
        .then((i) => !off && setInspections((cur) => ({ ...cur, [node]: i })))
        .catch((e) => !off && setInspections((cur) => ({ ...cur, [node]: e instanceof ApiError ? e.message : 'unreachable' })));
    }
    return () => {
      off = true;
    };
  }, [volume]);

  const deleteEverywhere = async () => {
    if (!volume) return;
    const [drive, name] = splitId(volume.id);
    const ok = await confirm({
      title: `Delete ${volume.id} everywhere?`,
      body: 'Every copy on every node is removed. This cannot be undone.',
      confirmLabel: `Delete ${volume.id}`,
      confirmVariant: 'danger',
      requireText: volume.id,
    });
    if (!ok) return;
    try {
      await api.deleteVolume(drive, name);
      toast(`Deleted ${volume.id}.`);
      onClose();
    } catch (e) {
      toast(e instanceof ApiError ? e.message : 'Could not delete the volume.', { tone: 'danger' });
    }
  };

  return (
    <BottomSheet
      open={!!volume}
      title={volume ? `Copies of ${volume.id}` : 'Copies'}
      onClose={onClose}
      footer={
        volume && (
          <>
            <Link to="/storage/backups"><Button icon="archive">Restore from backup</Button></Link>
            <Button variant="danger" icon="x" onClick={() => void deleteEverywhere()}>Delete {volume.id} everywhere</Button>
          </>
        )
      }
    >
      {volume && (
        <>
          <p className="cl-sm cl-muted">Generation {volume.generation}, primary on {volume.primary}.</p>
          {Object.entries(volume.replicas).map(([node, gen]) => {
            const i = inspections[node];
            return (
              <div className="li" key={node}>
                <strong>{node}</strong>
                {node === volume.primary && <Tag tone="pop">primary</Tag>}
                <Tag tone={gen === volume.generation ? 'ok' : 'warn'}>generation {gen}</Tag>
                <span className="cl-sm cl-muted" style={{ flex: 1 }}>
                  {i === undefined ? 'Checking…' : typeof i === 'string' ? i : `${i.files} files, ${formatBytes(i.bytes)}${i.newest_file_at ? `, newest ${relativeTime(i.newest_file_at * 1000)}` : ''}`}
                </span>
              </div>
            );
          })}
        </>
      )}
    </BottomSheet>
  );
}
