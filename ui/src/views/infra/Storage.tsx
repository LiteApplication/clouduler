// /storage (Volumes) and /storage/backups (Backups): one page, two tabs.
// Volumes holds the drives, their volumes and the per-node copies; Backups
// holds the restic schedules, runs, destinations, restores and snapshots.

import { useEffect, useState } from 'react';
import { Navigate, useNavigate, useParams } from 'react-router-dom';
import { api, ApiError } from '../../api/client';
import type { DriveStatus, VolumeState } from '../../api/types';
import { Banner, Button, EmptyState, Meter, PageHeader, Panel, Tabs, Tag } from '../../ds';
import { useNodeMetrics } from '../../hooks/useNodeMetrics';
import { formatBytes } from '../../lib/format';
import { useAuth } from '../../auth/AuthContext';
import { useClusterState } from '../../state/store';
import { Backups } from './Backups';
import { CompareCopies, StorageSection, VolumeBanners, VolumesTable } from './volumes';

const TABS = [
  { id: 'volumes', label: 'Volumes' },
  { id: 'backups', label: 'Backups' },
];

/** The node with the least free space for one drive, by the metrics sampler. */
export function fullestNode(drive: string, metrics: ReturnType<typeof useNodeMetrics>['metrics']) {
  let worst: { node: string; used: number; free: number } | null = null;
  for (const [node, u] of Object.entries(metrics)) {
    const d = u.drives?.find((x) => x.drive === drive);
    if (!d || d.total_bytes <= 0) continue;
    const used = (1 - d.free_bytes / d.total_bytes) * 100;
    if (!worst || used > worst.used) worst = { node, used, free: d.free_bytes };
  }
  return worst;
}

export function useDrives() {
  const [drives, setDrives] = useState<DriveStatus[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let off = false;
    const load = () =>
      api.listDrives().then((d) => !off && (setDrives(d), setError(null)), (e) => !off && setError(e instanceof ApiError ? e.message : String(e)));
    void load();
    const id = setInterval(load, 10_000);
    return () => {
      off = true;
      clearInterval(id);
    };
  }, []);
  return { drives, error };
}

export function Storage() {
  const { tab } = useParams();
  const navigate = useNavigate();
  if (tab !== undefined && tab !== 'backups') return <Navigate to="/storage" replace />;
  const active = tab === 'backups' ? 'backups' : 'volumes';

  return (
    <>
      <PageHeader
        eyebrow="Network & data"
        title="Storage"
        sub="Drives are places on your nodes. Volumes are the folders your containers keep their data in. Backups are restic snapshots of your drives."
      />
      <Tabs
        label="Storage sections"
        items={TABS}
        active={active}
        onChange={(id) => navigate(id === 'backups' ? '/storage/backups' : '/storage')}
      />
      <div className="tabpad">
        {active === 'volumes' ? <VolumesTab /> : <Backups />}
      </div>
    </>
  );
}

function VolumesTab() {
  const cluster = useClusterState();
  const navigate = useNavigate();
  const { isAdmin } = useAuth();
  const { drives, error } = useDrives();
  const [comparing, setComparing] = useState<VolumeState | null>(null);
  const names = Object.keys(cluster?.nodes ?? {});
  const { metrics } = useNodeMetrics(names);
  const volumes = cluster ? Object.values(cluster.volumes) : [];

  return (
    <>
      {error && <Banner tone="danger">{error}</Banner>}
      <VolumeBanners volumes={volumes} onCompare={setComparing} />

      <StorageSection
        id="drives-h"
        title="Drives"
        sub="Space left is the smallest free space among a drive's nodes. Its folder on the fullest node sets the limit."
      >
        {drives && drives.length === 0 && (
          <EmptyState icon="archive" title="No drives yet">Declare a drive under Settings, Storage to get volumes.</EmptyState>
        )}
        <div className="grid">
          {drives?.map((d) => {
            const fullest = fullestNode(d.name, metrics);
            const mine = volumes.filter((v) => v.id.startsWith(`${d.name}/`));
            return (
              <Panel key={d.name} title={d.name} actions={<Tag tone={d.mode === 'served' ? 'pop' : 'brand'}>{d.mode}</Tag>}>
                <dl className="kv">
                  <dt>Nodes</dt>
                  <dd>{d.nodes.map((n) => `${n.node}${n.reachable ? '' : ' (unreachable)'}`).join(', ') || 'none'}</dd>
                  <dt>Sync</dt>
                  <dd>every {d.sync_interval}</dd>
                  {d.mode === 'served' && (
                    <>
                      <dt>Active</dt>
                      <dd>{d.active_server ?? 'nobody reachable'}</dd>
                    </>
                  )}
                  <dt>Volumes</dt>
                  <dd>{mine.length}</dd>
                </dl>
                {fullest ? (
                  <Meter label={`Fullest node: ${fullest.node}, ${formatBytes(fullest.free)} left`} value={fullest.used} />
                ) : (
                  <p className="sub">Space left: unknown until a node reports.</p>
                )}
                <div className="acts">
                  <Button size="sm" onClick={() => navigate(`/storage/drives/${encodeURIComponent(d.name)}`)}>Open {d.name}</Button>
                </div>
              </Panel>
            );
          })}
        </div>
      </StorageSection>

      <StorageSection id="vol-h" title="Volumes" sub={isAdmin ? 'Sync, inspect or freeze a volume from its row.' : undefined}>
        <Panel flush>
          <VolumesTable volumes={volumes} onCompare={setComparing} />
        </Panel>
      </StorageSection>

      <CompareCopies volume={comparing} onClose={() => setComparing(null)} />
    </>
  );
}
