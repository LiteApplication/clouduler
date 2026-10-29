// /storage/drives/:name: one drive, its nodes, volumes and backup schedules.

import { useState } from 'react';
import { Link, useParams } from 'react-router-dom';
import type { VolumeState } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Banner, Button, KV, PageHeader, Panel, Tag } from '../../ds';
import { relativeTime } from '../../lib/format';
import { useClusterState } from '../../state/store';
import { useDrives } from './Storage';
import { DataTable } from './table';
import { CompareCopies, StorageSection, VolumeBanners, VolumesTable } from './volumes';
import './infra.css';

export function DriveDetail() {
  const { name = '' } = useParams();
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const { drives, error } = useDrives();
  const [comparing, setComparing] = useState<VolumeState | null>(null);
  const drive = drives?.find((d) => d.name === name);
  const volumes = cluster ? Object.values(cluster.volumes).filter((v) => v.id.startsWith(`${name}/`)) : [];
  const targets = cluster ? Object.values(cluster.restic_targets).filter((t) => t.drive === name) : [];

  if (error) return <Banner tone="danger">{error}</Banner>;
  if (!drive) return <p className="sub">{drives ? `No drive named ${name}.` : 'Loading drive…'}</p>;

  return (
    <>
      <PageHeader
        breadcrumb={[{ label: 'Storage', to: '/storage' }, { label: name }]}
        title={name}
        sub={`${drive.mode} · sync every ${drive.sync_interval}`}
        actions={isAdmin && <Link to="/storage/backups"><Button icon="archive">Restore from backup</Button></Link>}
      />
      <div className="tabpad infra-stack">
        <VolumeBanners volumes={volumes} onCompare={setComparing} />

        <div className="grid grid--2">
          <Panel title="Nodes" flush>
            <DataTable
              caption="Implementing nodes"
              rows={drive.nodes.map((n) => ({ ...n, id: n.node }))}
              empty="No node implements this drive."
              columns={[
                { key: 'node', header: 'Node', render: (n) => <strong>{n.node}</strong> },
                { key: 'reach', header: 'Reachable', render: (n) => <Tag tone={n.reachable ? 'ok' : 'danger'}>{n.reachable ? 'yes' : 'no'}</Tag> },
                ...(drive.mode === 'served' ? [{ key: 'prio', header: 'Priority', render: (n: { node: string }) => drive.served_by.indexOf(n.node) + 1 || '-' }] : []),
              ]}
            />
          </Panel>
          <Panel title="About">
            <KV items={[
              ['Mode', drive.mode],
              ['Sync interval', drive.sync_interval],
              ...(drive.mode === 'served' ? [['Active server', drive.active_server ?? 'none reachable'] as [string, string]] : []),
              ['Volumes', volumes.length],
            ]} />
          </Panel>
        </div>

        <StorageSection id="dvol-h" title="Volumes">
          <Panel flush>
            <VolumesTable volumes={volumes} onCompare={setComparing} showDrive={false} />
          </Panel>
        </StorageSection>

        <CompareCopies volume={comparing} onClose={() => setComparing(null)} />

        <StorageSection id="dbk-h" title="Backups">
          <Panel flush>
            <DataTable
              caption="Backup schedules for this drive"
              rows={targets.map((t) => ({ ...t, id: t.id }))}
              empty={<>No backup schedules for this drive. Add one on the <Link to="/storage/backups">Backups tab</Link>.</>}
              columns={[
                { key: 'dest', header: 'Destination', render: (t) => cluster?.restic_destinations[t.destination_id]?.name ?? t.destination_id },
                { key: 'freq', header: 'Frequency', render: (t) => `every ${Math.round(t.frequency_secs / 3600)}h` },
                { key: 'last', header: 'Last run', render: (t) => (t.last_run ? relativeTime(t.last_run) : 'never') },
                { key: 'en', header: 'State', render: (t) => <Tag tone={t.enabled ? 'ok' : undefined}>{t.enabled ? 'enabled' : 'paused'}</Tag> },
              ]}
            />
          </Panel>
        </StorageSection>
      </div>
    </>
  );
}
