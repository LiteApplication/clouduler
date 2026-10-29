// /storage: drives and their volumes. Backups live one page over.

import { useEffect, useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { api, ApiError } from '../../api/client';
import type { DriveStatus, VolumeState } from '../../api/types';
import { Banner, Button, EmptyState, Meter, PageHeader, Panel, Tag } from '../../ds';
import { useNodeMetrics } from '../../hooks/useNodeMetrics';
import { formatBytes } from '../../lib/format';
import { useClusterState } from '../../state/store';
import { CompareSheet, VolumeBanners, VolumesTable } from './volumes';

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
  const cluster = useClusterState();
  const navigate = useNavigate();
  const { drives, error } = useDrives();
  const [comparing, setComparing] = useState<VolumeState | null>(null);
  const names = Object.keys(cluster?.nodes ?? {});
  const { metrics } = useNodeMetrics(names);
  const volumes = cluster ? Object.values(cluster.volumes) : [];

  return (
    <>
      <PageHeader
        eyebrow="Network & data"
        title="Storage"
        sub={`${drives?.length ?? 0} drive${drives?.length === 1 ? '' : 's'} · ${volumes.length} volume${volumes.length === 1 ? '' : 's'}`}
        actions={<Link to="/storage/backups"><Button variant="primary" icon="archive">Backups</Button></Link>}
      />
      <div className="stack">
        {error && <Banner tone="danger">{error}</Banner>}
        <VolumeBanners volumes={volumes} onCompare={setComparing} />

        {drives && drives.length === 0 && (
          <EmptyState icon="archive" title="No drives yet">Declare a drive under Settings, Storage to get volumes.</EmptyState>
        )}
        <div className="grid">
          {drives?.map((d) => {
            const fullest = fullestNode(d.name, metrics);
            const mine = volumes.filter((v) => v.id.startsWith(`${d.name}/`));
            return (
              <Panel
                key={d.name}
                title={d.name}
                actions={<Tag tone={d.mode === 'served' ? 'warn' : 'brand'}>{d.mode}</Tag>}
              >
                <p className="cl-sm cl-muted">
                  {d.nodes.map((n) => `${n.node}${n.reachable ? '' : ' (unreachable)'}`).join(', ') || 'No nodes'} · every {d.sync_interval}
                  {d.mode === 'served' && ` · served by ${d.active_server ?? 'nobody reachable'}`}
                </p>
                {fullest ? (
                  <Meter label={`Fullest: ${fullest.node}, ${formatBytes(fullest.free)} left`} value={fullest.used} />
                ) : (
                  <p className="cl-sm cl-muted">Space left: -</p>
                )}
                <div className="row row--between">
                  <span className="cl-sm">{mine.length} volume{mine.length === 1 ? '' : 's'}</span>
                  <Button size="sm" onClick={() => navigate(`/storage/drives/${encodeURIComponent(d.name)}`)}>Open {d.name}</Button>
                </div>
              </Panel>
            );
          })}
        </div>

        <Panel title="Volumes" flush>
          <VolumesTable volumes={volumes} onCompare={setComparing} />
        </Panel>
      </div>
      <CompareSheet volume={comparing} onClose={() => setComparing(null)} />
    </>
  );
}
