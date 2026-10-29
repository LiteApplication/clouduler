// The three Backups forms, each in a BottomSheet: a schedule, a destination
// and a restore. Their drafts and request shapes are the old pages' own.

import { useEffect, useMemo, useState } from 'react';
import { api, ApiError } from '../../api/client';
import type { BackupDestinationKind, BackupDestinationSummary, DriveBackupTarget, DriveStatus, InstanceState, SnapshotInfo } from '../../api/types';
import { SelectField } from '../../components/FormFields';
import { Banner, BottomSheet, Button, Chip, TextField, Toggle, useConfirm, useToast } from '../../ds';

// ---------------------------------------------------------------- schedule

interface ScheduleDraft {
  drive: string;
  destination_id: string;
  enabled: boolean;
  frequency_hours: number;
  time_of_day: string;
  keep_last: number;
  keep_within_days: number;
  stop_containers: 'default' | 'yes' | 'no';
}

export function ScheduleSheet({ open, editing, drives, destinations, onClose, onSaved }: {
  open: boolean;
  editing: DriveBackupTarget | null;
  drives: DriveStatus[];
  destinations: BackupDestinationSummary[];
  onClose: () => void;
  onSaved: () => void;
}) {
  const toast = useToast();
  const blank = (): ScheduleDraft => ({ drive: drives[0]?.name ?? '', destination_id: destinations[0]?.id ?? '', enabled: true, frequency_hours: 24, time_of_day: '', keep_last: 7, keep_within_days: 0, stop_containers: 'default' });
  const [draft, setDraft] = useState<ScheduleDraft>(blank);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setError(null);
    setDraft(
      editing
        ? {
            drive: editing.drive,
            destination_id: editing.destination_id,
            enabled: editing.enabled,
            frequency_hours: Math.max(1, Math.round(editing.frequency_secs / 3600)),
            time_of_day: editing.time_of_day ?? '',
            keep_last: editing.keep_last ?? 0,
            keep_within_days: editing.keep_within_secs ? Math.round(editing.keep_within_secs / 86400) : 0,
            stop_containers: editing.stop_containers == null ? 'default' : editing.stop_containers ? 'yes' : 'no',
          }
        : blank(),
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, editing]);

  const save = async () => {
    setError(null);
    try {
      await api.putBackupTarget({
        id: editing?.id ?? `${draft.drive}-${draft.destination_id}`,
        drive: draft.drive,
        destination_id: draft.destination_id,
        enabled: draft.enabled,
        frequency_secs: Math.max(1, draft.frequency_hours) * 3600,
        time_of_day: draft.time_of_day || null,
        keep_last: draft.keep_last > 0 ? draft.keep_last : null,
        keep_within_secs: draft.keep_within_days > 0 ? draft.keep_within_days * 86400 : null,
        stop_containers: draft.stop_containers === 'default' ? null : draft.stop_containers === 'yes',
      });
      toast(editing ? 'Saved the schedule.' : 'Created the schedule.');
      onSaved();
      onClose();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Could not save that schedule.');
    }
  };

  const num = (label: string, key: 'frequency_hours' | 'keep_last' | 'keep_within_days', hint: string) => (
    <TextField label={label} type="number" inputMode="numeric" mono value={String(draft[key])} onChange={(e) => setDraft({ ...draft, [key]: Number(e.target.value) })} hint={hint} />
  );

  return (
    <BottomSheet open={open} title={editing ? 'Edit schedule' : 'New schedule'} onClose={onClose} footer={<Button variant="primary" icon="check" onClick={() => void save()}>{editing ? 'Save schedule' : 'Create schedule'}</Button>}>
      <SelectField id="sched-drive" label="Drive" value={draft.drive} onChange={(drive) => setDraft({ ...draft, drive })} options={drives.map((d) => ({ value: d.name, label: d.name }))} />
      <SelectField id="sched-dest" label="Destination" value={draft.destination_id} onChange={(destination_id) => setDraft({ ...draft, destination_id })} options={destinations.map((d) => ({ value: d.id, label: d.name }))} />
      {num('Frequency (hours)', 'frequency_hours', 'Minimum time between runs.')}
      <TextField label="Time of day (UTC)" type="time" mono value={draft.time_of_day} onChange={(e) => setDraft({ ...draft, time_of_day: e.target.value })} hint="Optional. Blank runs as soon as the frequency has elapsed." />
      {num('Keep last N snapshots', 'keep_last', '0 means no limit on count.')}
      {num('Keep snapshots within (days)', 'keep_within_days', '0 means no age-based limit.')}
      <SelectField
        id="sched-stop"
        label="Stop containers before backup"
        value={draft.stop_containers}
        onChange={(stop_containers) => setDraft({ ...draft, stop_containers })}
        options={[
          { value: 'default', label: "Use the stack's setting (default: yes)" },
          { value: 'yes', label: 'Always stop' },
          { value: 'no', label: 'Never stop' },
        ]}
      />
      <Toggle label="Enabled" checked={draft.enabled} onChange={(enabled) => setDraft({ ...draft, enabled })} />
      {error && <Banner tone="danger">{error}</Banner>}
    </BottomSheet>
  );
}

// ------------------------------------------------------------- destination

type Kind = BackupDestinationKind['kind'];
const KIND_LABEL: Record<Kind, string> = { nodelocal: 'Cluster node', ssh: 'SSH server', s3: 'S3 bucket' };

interface DestDraft {
  name: string;
  kind: Kind;
  node: string;
  path: string;
  host: string;
  port: number;
  user: string;
  bucket: string;
  endpoint: string;
  region: string;
  prefix: string;
  password: string;
  ssh_private_key: string;
  s3_access_key_id: string;
  s3_secret_access_key: string;
}

const blankDest = (node: string): DestDraft => ({
  name: '', kind: 'nodelocal', node, path: '/srv/backups', host: '', port: 22, user: '', bucket: '', endpoint: '', region: '', prefix: '',
  password: '', ssh_private_key: '', s3_access_key_id: '', s3_secret_access_key: '',
});

export function summarizeKind(kind: BackupDestinationKind): string {
  switch (kind.kind) {
    case 'nodelocal':
      return `${kind.node}:${kind.path}`;
    case 'ssh':
      return `${kind.user}@${kind.host}:${kind.port}${kind.path}`;
    case 's3':
      return `${kind.bucket}${kind.prefix ? `/${kind.prefix}` : ''} @ ${kind.endpoint}`;
  }
}
export { KIND_LABEL };

export function DestinationSheet({ open, editing, nodeNames, onClose, onSaved }: {
  open: boolean;
  editing: BackupDestinationSummary | null;
  nodeNames: string[];
  onClose: () => void;
  onSaved: () => void;
}) {
  const toast = useToast();
  const [draft, setDraft] = useState<DestDraft>(blankDest(''));
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setError(null);
    const base = blankDest(nodeNames[0] ?? '');
    if (!editing) return setDraft(base);
    const k = editing.kind;
    if (k.kind === 'nodelocal') setDraft({ ...base, name: editing.name, kind: 'nodelocal', node: k.node, path: k.path });
    else if (k.kind === 'ssh') setDraft({ ...base, name: editing.name, kind: 'ssh', host: k.host, port: k.port, user: k.user, path: k.path });
    else setDraft({ ...base, name: editing.name, kind: 's3', bucket: k.bucket, endpoint: k.endpoint, region: k.region ?? '', prefix: k.prefix ?? '' });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, editing]);

  const save = async () => {
    setError(null);
    const id = editing?.id ?? (draft.name.trim().toLowerCase().replace(/[^a-z0-9-]+/g, '-') || `dest-${Date.now()}`);
    const kind: BackupDestinationKind =
      draft.kind === 'nodelocal'
        ? { kind: 'nodelocal', node: draft.node, path: draft.path }
        : draft.kind === 'ssh'
          ? { kind: 'ssh', host: draft.host, port: draft.port, user: draft.user, path: draft.path }
          : { kind: 's3', bucket: draft.bucket, endpoint: draft.endpoint, region: draft.region || null, prefix: draft.prefix || null };
    try {
      await api.putBackupDestination({
        id,
        name: draft.name,
        kind,
        password: draft.password || undefined,
        ssh_private_key: draft.kind === 'ssh' && draft.ssh_private_key ? draft.ssh_private_key : undefined,
        s3_access_key_id: draft.kind === 's3' && draft.s3_access_key_id ? draft.s3_access_key_id : undefined,
        s3_secret_access_key: draft.kind === 's3' && draft.s3_secret_access_key ? draft.s3_secret_access_key : undefined,
      });
      toast(editing ? 'Saved the destination.' : 'Created the destination.');
      onSaved();
      onClose();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Could not save that destination.');
    }
  };

  const f = (label: string, key: keyof DestDraft, extra?: { hint?: string; type?: string; multiline?: boolean }) => (
    <TextField
      label={label}
      mono
      type={extra?.type}
      multiline={extra?.multiline}
      value={String(draft[key])}
      onChange={(e) => setDraft({ ...draft, [key]: extra?.type === 'number' ? Number(e.target.value) : e.target.value })}
      hint={extra?.hint}
    />
  );

  return (
    <BottomSheet open={open} title={editing ? 'Edit destination' : 'New destination'} onClose={onClose} footer={<Button variant="primary" icon="check" onClick={() => void save()}>{editing ? 'Save destination' : 'Create destination'}</Button>}>
      {f('Name', 'name', { hint: 'Shown in schedules and history.' })}
      <div className="chips" role="group" aria-label="Destination kind">
        {(Object.keys(KIND_LABEL) as Kind[]).map((k) => (
          <Chip key={k} label={KIND_LABEL[k]} selected={draft.kind === k} onClick={() => setDraft({ ...draft, kind: k })} />
        ))}
      </div>
      {draft.kind === 'nodelocal' && (
        <>
          <SelectField id="dest-node" label="Node" value={draft.node} onChange={(node) => setDraft({ ...draft, node })} options={nodeNames.map((n) => ({ value: n, label: n }))} hint="If this node doesn't hold the drive, it is synced there once before each backup." />
          {f('Path', 'path', { hint: 'Directory on that node for the restic repository.' })}
        </>
      )}
      {draft.kind === 'ssh' && (
        <>
          {f('Host', 'host')}
          {f('Port', 'port', { type: 'number' })}
          {f('User', 'user')}
          {f('Path', 'path', { hint: 'Directory on the remote host for the restic repository.' })}
          {f('Private key', 'ssh_private_key', { multiline: true, hint: editing ? 'Leave blank to keep the current key.' : 'Sealed and stored. Never shown again.' })}
        </>
      )}
      {draft.kind === 's3' && (
        <>
          {f('Bucket', 'bucket')}
          {f('Endpoint', 'endpoint', { hint: 'For example s3.amazonaws.com, or a compatible provider.' })}
          {f('Region', 'region')}
          {f('Prefix', 'prefix')}
          {f('Access key ID', 's3_access_key_id', { hint: editing ? 'Leave blank to keep the current credentials.' : undefined })}
          <TextField label="Secret access key" type="password" mono value={draft.s3_secret_access_key} onChange={(e) => setDraft({ ...draft, s3_secret_access_key: e.target.value })} />
        </>
      )}
      <TextField label="Repository password" type="password" mono value={draft.password} onChange={(e) => setDraft({ ...draft, password: e.target.value })} hint={editing ? 'Leave blank to keep the current password.' : 'Leave blank to generate one.'} />
      {error && <Banner tone="danger">{error}</Banner>}
    </BottomSheet>
  );
}

// ----------------------------------------------------------------- restore

type Scope = 'drive' | 'volume' | 'service';

function parseInstanceId(id: string): { stack: string; service: string } | null {
  const parts = id.split('.');
  if (parts.length < 3) return null;
  return { stack: parts.slice(0, parts.length - 2).join('.'), service: parts[parts.length - 2] };
}

export interface RestorePreset {
  scope?: Scope;
  drive?: string;
  volume?: string;
  destinationId?: string;
  snapshot?: string;
}

export function RestoreSheet({ open, preset, drives, destinations, onClose }: {
  open: boolean;
  preset: RestorePreset;
  drives: DriveStatus[];
  destinations: BackupDestinationSummary[];
  onClose: () => void;
}) {
  const confirm = useConfirm();
  const toast = useToast();
  const [scope, setScope] = useState<Scope>('drive');
  const [drive, setDrive] = useState('');
  const [volume, setVolume] = useState('');
  const [stack, setStack] = useState('');
  const [service, setService] = useState('');
  const [destinationId, setDestinationId] = useState('');
  const [snapshot, setSnapshot] = useState('');
  const [instances, setInstances] = useState<InstanceState[]>([]);
  const [snapshots, setSnapshots] = useState<SnapshotInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setScope(preset.scope ?? 'drive');
    setDrive(preset.drive ?? drives[0]?.name ?? '');
    setVolume(preset.volume ?? '');
    setStack('');
    setService('');
    setDestinationId(preset.destinationId ?? destinations[0]?.id ?? '');
    setSnapshot(preset.snapshot ?? '');
    setError(null);
    api.listInstances().then(setInstances).catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, preset]);

  useEffect(() => {
    if (!open || !destinationId) return setSnapshots(null);
    api.listSnapshots(destinationId).then(setSnapshots).catch(() => setSnapshots([]));
  }, [open, destinationId]);

  const stacks = useMemo(() => [...new Set(instances.map((i) => parseInstanceId(i.id)?.stack).filter((s): s is string => !!s))], [instances]);
  const services = useMemo(() => [...new Set(instances.filter((i) => parseInstanceId(i.id)?.stack === stack).map((i) => parseInstanceId(i.id)!.service))], [instances, stack]);
  const target = scope === 'drive' ? drive : scope === 'volume' ? `${drive}/${volume}` : `${stack}/${service}`;
  const ready = scope === 'drive' ? !!drive : scope === 'volume' ? !!drive && !!volume : !!stack && !!service;

  const submit = async () => {
    const ok = await confirm({
      title: `Restore ${target}?`,
      body: `This overwrites the data there with ${snapshot ? `snapshot ${snapshot.slice(0, 8)}` : 'the latest snapshot'}. Containers using it are stopped first and restarted when the restore finishes.`,
      confirmLabel: `Restore ${target}`,
      confirmVariant: 'danger',
      requireText: target,
    });
    if (!ok) return;
    try {
      if (scope === 'drive') await api.restoreDrive({ drive, destination_id: destinationId, snapshot: snapshot || null });
      else if (scope === 'volume') await api.restoreVolume({ drive, volume: `${drive}/${volume}`, destination_id: destinationId, snapshot: snapshot || null });
      else await api.restoreService({ stack, service, destination_id: destinationId || null, snapshot: snapshot || null });
      toast(`Restoring ${target}. Follow it under Restores.`);
      onClose();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Could not submit the restore.');
    }
  };

  return (
    <BottomSheet open={open} title="Restore from backup" onClose={onClose} footer={<Button variant="danger" icon="x" disabled={!ready || (scope !== 'service' && !destinationId)} onClick={() => void submit()}>Restore {ready ? target : ''}</Button>}>
      <div className="chips" role="group" aria-label="Restore scope">
        {(['drive', 'volume', 'service'] as Scope[]).map((s) => (
          <Chip key={s} label={s === 'drive' ? 'Whole drive' : s === 'volume' ? 'Single volume' : "A service's volumes"} selected={scope === s} onClick={() => setScope(s)} />
        ))}
      </div>
      {(scope === 'drive' || scope === 'volume') && <SelectField id="restore-drive" label="Drive" value={drive} onChange={setDrive} options={drives.map((d) => ({ value: d.name, label: d.name }))} />}
      {scope === 'volume' && <TextField label="Volume" mono value={volume} onChange={(e) => setVolume(e.target.value)} hint={`Under drive ${drive || '…'}`} />}
      {scope === 'service' && (
        <>
          <SelectField id="restore-stack" label="Stack" value={stack} onChange={(s) => { setStack(s); setService(''); }} options={[{ value: '', label: 'Pick a stack' }, ...stacks.map((s) => ({ value: s, label: s }))]} />
          <SelectField id="restore-service" label="Service" value={service} onChange={setService} options={[{ value: '', label: 'Pick a service' }, ...services.map((s) => ({ value: s, label: s }))]} />
        </>
      )}
      <SelectField
        id="restore-destination"
        label={scope === 'service' ? 'Destination (optional)' : 'Destination'}
        value={destinationId}
        onChange={setDestinationId}
        options={[...(scope === 'service' ? [{ value: '', label: 'Resolve automatically' }] : []), ...destinations.map((d) => ({ value: d.id, label: d.name }))]}
      />
      <SelectField
        id="restore-snapshot"
        label="Snapshot"
        value={snapshot}
        onChange={setSnapshot}
        options={[{ value: '', label: 'Latest' }, ...(snapshots ?? []).map((s) => ({ value: s.id, label: `${new Date(s.time).toLocaleString()} · ${s.id.slice(0, 8)}` }))]}
      />
      <Banner tone="warn">Restoring overwrites what is there now. You will be asked to type the target's name.</Banner>
      {error && <Banner tone="danger">{error}</Banner>}
    </BottomSheet>
  );
}
