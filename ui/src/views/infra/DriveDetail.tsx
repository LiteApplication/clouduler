// /storage/drives/:name: one drive, its nodes, volumes and backup schedules.

import { useState } from 'react';
import { Link, useParams } from 'react-router-dom';
import { ApiError, api } from '../../api/client';
import type { VolumeState } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Banner, Button, KV, PageHeader, Panel, Tag, useConfirm, useToast } from '../../ds';
import { relativeTime } from '../../lib/format';
import { useClusterState } from '../../state/store';
import { useDrives } from './Storage';
import { DataTable } from './table';
import { CompareCopies, StorageSection, VolumeBanners, VolumesTable } from './volumes';

export function DriveDetail() {
  const { name = '' } = useParams();
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const { drives, error } = useDrives();
  const toast = useToast();
  const confirm = useConfirm();
  const [comparing, setComparing] = useState<VolumeState | null>(null);
  const drive = drives?.find((d) => d.name === name);
  const volumes = cluster ? Object.values(cluster.volumes).filter((v) => v.id.startsWith(`${name}/`)) : [];
  const targets = cluster ? Object.values(cluster.restic_targets).filter((t) => t.drive === name) : [];

  const resolve = async (keep: string, other: string) => {
    const ok = await confirm({
      title: `Keep the copy on ${keep}?`,
      body: `The copy on ${other} is overwritten with the copy on ${keep}. Save anything you need from ${other} first.`,
      confirmLabel: `Keep ${keep}`,
      confirmVariant: 'danger',
    });
    if (!ok) return;
    try {
      await api.resolveDrive(name, keep);
      toast(`Keeping the copy on ${keep}.`);
    } catch (e) {
      toast(e instanceof ApiError ? e.message : 'Could not resolve the conflict.', { tone: 'danger' });
    }
  };
  const failBack = async () => {
    try {
      await api.failbackDrive(name);
      toast(`Handing ${name} back.`);
    } catch (e) {
      toast(e instanceof ApiError ? e.message : 'Could not fail back.', { tone: 'danger' });
    }
  };

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
      <div className="tabpad stack">
        {drive.served?.conflict && (
          <Banner tone="danger" title={`${name} has diverged copies`}>
            The copies on {drive.served.conflict.holder} and {drive.served.conflict.other} were both changed while they were apart.
            Nothing is synced between them and {drive.served.conflict.holder} keeps serving. Keep one copy: the other is overwritten,
            so save anything you need from it first. {drive.served.conflict.reason}
            {isAdmin && (
              <span className="acts">
                {[drive.served.conflict.holder, drive.served.conflict.other].map((keep, i, both) => (
                  <Button key={keep} size="sm" onClick={() => void resolve(keep, both[1 - i])}>Keep {keep}</Button>
                ))}
              </span>
            )}
          </Banner>
        )}
        {drive.served?.failback_blocked && (
          <Banner tone="warn" title={`${name} could not fail back`}>{drive.served.failback_blocked}</Banner>
        )}
        {isAdmin && drive.mode === 'served' && drive.served && drive.served.holder !== drive.served_by[0]
          && !drive.served.conflict && !drive.served.handover_to && (
          <div><Button onClick={() => void failBack()}>Fail back now</Button></div>
        )}
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
              ...(drive.mode === 'served' ? [['Fail back', drive.failback] as [string, string]] : []),
              ...(drive.served ? [['Holder', drive.served.holder], ['Handover', drive.served.handover_to ?? '-']] as [string, string][] : []),
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
