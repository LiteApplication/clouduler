// /nodes/:name: one node's health, resources, config and logs, plus the
// drain / activate / retire actions.

import { useEffect, useMemo, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { api, ApiError, nodeLogsUrl } from '../../api/client';
import type { NodeAddress, NodeDetail as NodeDetailT } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Banner, Button, ContainerRow, LogView, Meter, PageHeader, Panel, KV, StatusBadge, Tag, TextField, useConfirm, useToast, type LogViewLine } from '../../ds';
import { useEventSource } from '../../hooks/useEventSource';
import { cpuPercent, memPercent } from '../../hooks/useNodeMetrics';
import { formatBytes, formatUptime } from '../../lib/format';
import { instanceStatus, nodeStatus } from '../../lib/status';
import { useInstances } from '../../state/instancesStore';
import { useClusterState } from '../../state/store';
import { useConfigApply, useConfigDraft } from '../config/draft';
import { NodeCard as NodeConfigForm } from '../config/sections';

export function NodeDetail() {
  const { name = '' } = useParams();
  const navigate = useNavigate();
  const cluster = useClusterState();
  const instances = useInstances();
  const { isAdmin } = useAuth();
  const confirm = useConfirm();
  const toast = useToast();
  const [detail, setDetail] = useState<NodeDetailT | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState(false);
  const [rollbackAfter, setRollbackAfter] = useState(60);
  const { original, draft, setDraft, reload } = useConfigDraft();
  const { errors: configErrors, pending, busy, apply, confirm: confirmConfig, rollback: rollbackConfig, clearErrors } = useConfigApply(reload);

  useEffect(() => {
    let cancelled = false;
    const load = () =>
      api
        .getNode(name)
        .then((d) => !cancelled && setDetail(d))
        .catch((e) => !cancelled && setError(e instanceof ApiError ? e.message : String(e)));
    load();
    const id = setInterval(load, 5000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [name]);

  const node = cluster?.nodes[name];
  const configNode = draft?.nodes[name];
  const mine = useMemo(() => (instances ?? []).filter((i) => i.node === name), [instances, name]);
  const assigned = cluster ? Object.values(cluster.assignments).filter((a) => a.node === name).length : 0;
  const remaining = Math.max(mine.length, assigned);
  // A node the config declares but the live roster hasn't admitted yet has
  // nothing in `cluster.nodes`, so its facts fall back to the config entry and
  // the raft-only actions (drain, activate) stay hidden.
  const role = node?.role ?? configNode?.role ?? '';
  const unreachable = !!cluster?.unreachable.includes(name);
  const view = node ? nodeStatus(node, unreachable) : null;
  const usage = detail?.usage;

  const setAvailability = async (to: 'active' | 'drain') => {
    if (to === 'drain') {
      const ok = await confirm({
        title: `Drain ${name}?`,
        body: 'Its containers move to other nodes and nothing new is placed here.',
        confirmLabel: `Drain ${name}`,
        confirmVariant: 'primary',
      });
      if (!ok) return;
    } else if (!(await confirm({ title: `Activate ${name}?`, body: 'New containers can be placed here again.', confirmLabel: `Activate ${name}`, confirmVariant: 'primary' }))) {
      return;
    }
    try {
      await api.setAvailability(name, to);
      toast(to === 'drain' ? `Draining ${name}.` : `${name} is active.`);
    } catch (e) {
      toast(e instanceof ApiError ? e.message : 'Could not change availability.', { tone: 'danger' });
    }
  };

  const retire = async () => {
    const ok = await confirm({
      title: `Remove ${name}?`,
      body: 'This takes the node out of the cluster for good. To use the machine again, run init on it.',
      confirmLabel: `Remove ${name}`,
      confirmVariant: 'danger',
      requireText: name,
    });
    if (!ok) return;
    try {
      await api.removeNode(name);
      navigate('/nodes');
    } catch (e) {
      toast(e instanceof ApiError ? e.message : 'Could not remove the node.', { tone: 'danger' });
    }
  };

  const cancelEditing = () => {
    // Drop only this node's staged edits; unrelated drafts elsewhere stay put.
    if (original && draft) setDraft({ ...draft, nodes: { ...draft.nodes, [name]: original.nodes[name] } });
    clearErrors();
    setEditing(false);
  };
  const save = async () => {
    if (draft && (await apply(draft, `${rollbackAfter}s`))) setEditing(false);
  };

  if (!node && !configNode) {
    if (!cluster || !draft) return <p className="cl-muted">Loading node…</p>;
    return <Banner tone="warn" title={`No node named ${name}`}>Check the name on the Nodes page.</Banner>;
  }

  const diskOver = usage?.drives?.find((d) => d.total_bytes > 0 && 1 - d.free_bytes / d.total_bytes > 0.9);
  const cpu = usage ? cpuPercent(usage) : null;
  const mem = usage ? memPercent(usage) : null;

  return (
    <>
      <PageHeader
        breadcrumb={[{ label: 'Nodes', to: '/nodes' }, { label: name }]}
        title={name}
        sub={<code>{node?.rpc_addr ?? 'not yet reachable'}</code>}
        actions={
          isAdmin && node && (
            node.availability === 'active' ? (
              <Button icon="pause" onClick={() => void setAvailability('drain')}>Drain {name}</Button>
            ) : (
              <Button variant="primary" icon="play" onClick={() => void setAvailability('active')}>Activate {name}</Button>
            )
          )
        }
      />
      <div className="row" style={{ marginBottom: 'var(--space-3)' }}>
        {view ? <StatusBadge status={view.status} label={view.label} /> : <Tag>pending</Tag>}
        <Tag>{role}</Tag>
        {Object.entries(node?.labels ?? {}).map(([k, v]) => <Tag key={k}>{k}={v}</Tag>)}
      </div>

      <div className="stack">
        {error && <Banner tone="warn">{error}</Banner>}
        {diskOver && (
          <Banner tone="danger" title="Disk almost full">
            {diskOver.drive} has {formatBytes(diskOver.free_bytes)} left. Free some space or move volumes elsewhere.
          </Banner>
        )}
        {pending?.armed && (
          <Banner tone="warn" title="Config change waiting for you" action="Roll back now" onAction={() => void rollbackConfig()}>
            {pending.seconds_left ?? '?'}s left to confirm. <Button size="sm" variant="primary" onClick={() => void confirmConfig()}>Confirm config</Button>
          </Banner>
        )}

        <div className="grid grid--2">
          <Panel title="Resources">
            {usage?.local || usage?.cpu_percent != null ? (
              <>
                {cpu != null && <Meter label="CPU" value={cpu} />}
                {mem != null && usage && (
                  <Meter label={`Memory ${formatBytes(usage.memory_used_bytes)} / ${formatBytes(usage.memory_total_bytes)}`} value={mem} />
                )}
                {usage?.drives?.map((d) => (
                  <Meter key={d.drive} label={`Disk ${d.drive}`} value={d.total_bytes > 0 ? (1 - d.free_bytes / d.total_bytes) * 100 : 0} />
                ))}
                <KV items={[
                  ['Load (1m)', usage ? `${usage.load_1m.toFixed(2)} on ${usage.cpu_count} cores` : '-'],
                  ['Uptime', usage?.uptime_seconds ? formatUptime(usage.uptime_seconds) : '-'],
                  ['Version', usage?.version ?? '-'],
                  ['Raft RPC', node?.rpc_addr ?? '-'],
                ]} />
              </>
            ) : (
              <p className="cl-sm cl-muted">Live numbers aren't available for this node yet.</p>
            )}
          </Panel>

          <Panel
            title="Networks and drives"
            actions={isAdmin && !editing && configNode && <Button size="sm" icon="edit" onClick={() => { clearErrors(); setEditing(true); }}>Edit</Button>}
          >
            {!draft && <p className="cl-muted">Loading configuration…</p>}
            {draft && !configNode && <p className="cl-sm cl-muted">This node is not in the cluster config yet.</p>}
            {configNode && !editing && (
              <>
                <KV items={[
                  ['Role', configNode.role],
                  ['RPC port', configNode.rpc_port],
                  ['Data dir', configNode.data_dir ?? 'cluster default'],
                  ['Cluster key', configNode.generate_secret_key == null ? 'follows cluster setting' : configNode.generate_secret_key ? 'creates' : 'never creates'],
                  ['Scheduling', configNode.last_resort ? 'last resort' : 'normal'],
                ]} />
                {Object.entries(configNode.network).map(([net, addr]) => (
                  <div className="li" key={net}><strong>{net}</strong><code>{describeAddress(addr)}</code></div>
                ))}
                {Object.keys(configNode.network).length === 0 && <p className="cl-sm cl-muted">No networks configured.</p>}
                {Object.entries(configNode.drives).map(([drive, path]) => (
                  <div className="li" key={drive}><strong>{drive}</strong><code>{path}</code></div>
                ))}
                {Object.keys(configNode.drives).length === 0 && <p className="cl-sm cl-muted">No drives configured.</p>}
              </>
            )}
          </Panel>
        </div>

        {configNode && editing && draft && (
          <Panel title="Edit node config">
            <div className="cfg">
              <NodeConfigForm
                name={name}
                node={configNode}
                networks={draft.networks}
                drives={draft.drives}
                onChange={(patch) => setDraft({ ...draft, nodes: { ...draft.nodes, [name]: { ...configNode, ...patch } } })}
                onRemove={async () => {
                  if (!(await confirm({ title: `Remove ${name} from the cluster config?`, body: 'This does not remove the node itself.', confirmLabel: 'Remove from config', confirmVariant: 'danger' }))) return;
                  const next = { ...draft.nodes };
                  delete next[name];
                  setDraft({ ...draft, nodes: next });
                }}
                disabled={!isAdmin || busy}
              />
            </div>
            {configErrors.length > 0 && (
              <Banner tone="danger" title="Fix these before saving">
                {configErrors.map((e, i) => (
                  <span key={i} style={{ display: 'block' }}>{e.path && <code>{e.path}: </code>}{e.message}</span>
                ))}
              </Banner>
            )}
            <TextField
              label="Rollback after (seconds)"
              type="number"
              inputMode="numeric"
              mono
              value={String(rollbackAfter)}
              onChange={(e) => setRollbackAfter(Math.max(5, Number(e.target.value) || 60))}
              hint="Network-affecting edits roll back unless you confirm in time."
            />
            <div className="row">
              <Button onClick={cancelEditing} disabled={busy}>Cancel</Button>
              <Button variant="primary" icon="check" loading={busy} onClick={() => void save()}>Save</Button>
            </div>
          </Panel>
        )}

        <Panel title={`Containers on ${name}`} flush>
          {mine.length === 0 && <p className="cl-sm cl-muted" style={{ padding: 'var(--space-4)' }}>Nothing is running here.</p>}
          {mine.map((i) => {
            const v = instanceStatus(i);
            return (
              <ContainerRow
                key={i.id}
                name={i.id}
                image={i.image ?? ''}
                status={v.status}
                onOpen={() => navigate(`/instances/${encodeURIComponent(i.id)}`)}
              />
            );
          })}
        </Panel>

        <DaemonLogs name={name} />

        {isAdmin && node && (
          <Panel title="Retire this node">
            <p className="cl-sm cl-muted">
              Removing a node takes it out of the cluster for good.
              {remaining > 0 ? ` ${name} still runs ${remaining} container${remaining === 1 ? '' : 's'}. Drain it first.` : ''}
            </p>
            <Button variant="danger" icon="x" disabled={remaining > 0} onClick={() => void retire()}>Remove {name}</Button>
          </Panel>
        )}
      </div>
    </>
  );
}

function describeAddress(addr: NodeAddress): string {
  if (typeof addr === 'string') return addr;
  const ports = addr.open_ports.map((p) => `${p.port}/${p.protocol}`).join(', ');
  if ('interface' in addr) return ports ? `${addr.interface} (open: ${ports})` : addr.interface;
  const parts = [addr.ipv4, addr.ipv6, addr.domain].filter((v): v is string => !!v);
  const base = parts.length > 0 ? parts.join(', ') : '(no address set)';
  return ports ? `${base} — open: ${ports}` : base;
}

interface DaemonLine {
  timestamp?: string;
  level?: string;
  message: string;
}

function levelOf(l: DaemonLine): LogViewLine['level'] {
  const s = (l.level ?? /\b(ERROR|WARN|DEBUG|INFO)\b/.exec(l.message)?.[1] ?? 'info').toLowerCase();
  return s === 'error' || s === 'warn' || s === 'debug' ? s : 'info';
}

function DaemonLogs({ name }: { name: string }) {
  const { isAdmin } = useAuth();
  const [open, setOpen] = useState(false);
  const { items, connected } = useEventSource<DaemonLine>(nodeLogsUrl(name), {
    limit: 500,
    enabled: open && isAdmin,
    events: ['log'],
  });
  const lines: LogViewLine[] = items.map((l) => ({
    t: (l.timestamp ?? '').slice(11, 19),
    level: levelOf(l),
    msg: l.message,
  }));
  return (
    <Panel title="Daemon logs" actions={isAdmin && <Button size="sm" onClick={() => setOpen((v) => !v)}>{open ? 'Hide logs' : 'Show logs'}</Button>}>
      {!isAdmin && <p className="cl-sm cl-muted">Daemon logs are visible to admins.</p>}
      {open && (
        <>
          <p className="cl-sm cl-muted">{connected ? 'Streaming.' : 'Connecting…'}</p>
          <LogView lines={lines} label={`${name} daemon logs`} />
        </>
      )}
    </Panel>
  );
}
