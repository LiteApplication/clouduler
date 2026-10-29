import { useEffect, useMemo, useState } from 'react';
import { Link, useLocation, useNavigate, useParams } from 'react-router-dom';
import { api, ApiError } from '../../api/client';
import type { InstanceState, StackRequirements, UpdateRow } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import {
  Banner,
  BottomSheet,
  Button,
  Chip,
  EmptyState,
  Icon,
  KV,
  LogView,
  PageHeader,
  Panel,
  Spark,
  StatusBadge,
  StepBar,
  Tag,
  TextField,
  useConfirm,
  useToast,
} from '../../ds';
import { useEventSource } from '../../hooks/useEventSource';
import { useStatsHistory } from '../../hooks/useStatsHistory';
import { formatBytes, formatDuration, relativeTime, shortDigest } from '../../lib/format';
import { buildPlacementRows, nodeEligibility } from '../../lib/placement';
import { instanceStatus, volumeStatus } from '../../lib/status';
import { useInstanceProgress } from '../../state/progressStore';
import { useClusterState } from '../../state/store';
import { logStreamOptions, toLogViewLine, type StreamLine } from './logLines';
import { usePlacementContext } from './specTools';
import './workloads.css';

/** `stack.service.index`; split from the right like the server does, since a
 *  stack name may itself contain a dot. */
function parseInstanceId(id: string): { stack: string; service: string; index: string } | null {
  const parts = id.split('.');
  if (parts.length < 3) return null;
  return { stack: parts.slice(0, -2).join('.'), service: parts[parts.length - 2], index: parts[parts.length - 1] };
}

export function ContainerDetail() {
  const { id = '' } = useParams();
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const navigate = useNavigate();
  const toast = useToast();
  const confirm = useConfirm();
  const progress = useInstanceProgress(id);
  const parsed = parseInstanceId(id);
  const [instance, setInstance] = useState<InstanceState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [update, setUpdate] = useState<UpdateRow | null>(null);
  const [requirements, setRequirements] = useState<StackRequirements | null>(null);
  const [moveOpen, setMoveOpen] = useState(false);

  useEffect(() => {
    let cancelled = false;
    const load = () =>
      api
        .getInstance(id)
        .then((i) => !cancelled && (setInstance(i), setError(null)))
        .catch((e) => !cancelled && setError(e instanceof ApiError ? e.message : String(e)));
    void load();
    const timer = window.setInterval(load, 4000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [id]);

  useEffect(() => {
    let cancelled = false;
    api
      .listUpdates?.()
      ?.then((rows) => !cancelled && setUpdate(rows.find((r) => r.id === id) ?? null))
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [id]);

  const stackSpec = parsed ? cluster?.stacks[parsed.stack]?.spec : undefined;
  useEffect(() => {
    if (!parsed || stackSpec == null) return;
    let cancelled = false;
    api
      .validateStack(parsed.stack, stackSpec)
      .then((r) => !cancelled && setRequirements(r.per_service[parsed.service] ?? null))
      .catch(() => {});
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [parsed?.stack, parsed?.service, stackSpec]);

  const run = async (what: string, action: () => Promise<unknown>, done: string) => {
    setBusy(what);
    try {
      await action();
      toast(done);
    } catch (e) {
      toast(`Could not ${what.toLowerCase()} ${id}`, { tone: 'danger', body: e instanceof ApiError ? e.message : undefined });
    } finally {
      setBusy(null);
    }
  };

  if (!instance) {
    return error ? (
      <EmptyState icon="container" title="Could not load this container" action={<Button variant="primary" onClick={() => navigate('/instances')}>Back to containers</Button>}>
        {error}
      </EmptyState>
    ) : (
      <p className="sub" role="status">
        Loading container...
      </p>
    );
  }

  const view = instanceStatus(instance);
  const stopped = instance.desired === 'stopped';
  const migration = instance.migration;
  const phase = migration && typeof migration.phase === 'string' ? migration.phase : null;

  const stop = async () => {
    if (!(await confirm({ title: `Stop ${id}?`, body: 'The container stops and stays stopped until you start it again.', confirmLabel: `Stop ${id}`, confirmVariant: 'danger' }))) return;
    await run('Stop', () => api.setDesired(id, 'stopped'), `Stopping ${id}`);
  };

  return (
    <div className="sec wl-page">
      <PageHeader
        breadcrumb={[
          { label: 'Stacks', to: '/stacks' },
          ...(parsed ? [{ label: parsed.stack, to: `/stacks/${encodeURIComponent(parsed.stack)}` }] : []),
          { label: parsed?.service ?? id },
        ]}
        title={id}
        sub={`on ${instance.node}`}
        actions={
          isAdmin ? (
            <>
              {stopped ? (
                <Button variant="primary" icon="play" loading={busy === 'Start'} onClick={() => run('Start', () => api.setDesired(id, 'running'), `Starting ${id}`)}>
                  {`Start ${id}`}
                </Button>
              ) : (
                <Button icon="restart" loading={busy === 'Restart'} onClick={() => run('Restart', () => api.restartInstance(id), `Restarting ${id}`)}>
                  {`Restart ${id}`}
                </Button>
              )}
              <Button icon="move" onClick={() => setMoveOpen(true)}>
                Move to another node
              </Button>
              {!stopped && (
                <Button variant="ghost" icon="stop" onClick={stop}>
                  {`Stop ${id}`}
                </Button>
              )}
            </>
          ) : undefined
        }
      />

      {error && (
        <Banner tone="warn" title="Showing the last known state">
          {error}
        </Banner>
      )}
      {instance.oom_killed && (
        <Banner tone="danger" title="Out of memory">
          {`The kernel killed ${id} for going over its memory limit${instance.memory_limit_bytes ? ` of ${formatBytes(instance.memory_limit_bytes)}` : ''}. Raise the limit in the stack spec and redeploy.`}
        </Banner>
      )}

      <Panel>
        <div className="row row--between">
          <div className="row">
            <StatusBadge status={view.status} label={view.label} />
            <Tag>{`Desired: ${instance.desired}`}</Tag>
            {instance.exit_code != null && <Tag tone={instance.exit_code === 0 ? undefined : 'danger'}>{`Exit code ${instance.exit_code}`}</Tag>}
            {instance.restart_count ? (
              <Tag tone="warn">{`Restart ${instance.restart_count}${instance.restart_limit ? ` of ${instance.restart_limit}` : ''}`}</Tag>
            ) : null}
            {instance.crash_moves ? <Tag tone="warn">{`Moved ${instance.crash_moves}× after crashes`}</Tag> : null}
          </div>
          {progress?.reason && (
            <span className="sub wl-inline">
              {progress.step && <StepBar step={progress.step[0]} total={progress.step[1]} />}
              {progress.reason}
            </span>
          )}
        </div>
        {instance.placement_error && (
          <p className="sub wl-danger">
            <Icon name="x" size={14} /> {`No node can run this: ${instance.placement_error}`}
          </p>
        )}
        {phase && phase !== 'done' && (
          <p className="sub">{`Moving to ${migration!.to}: ${phase}`}</p>
        )}
        <KV
          items={[
            ['Node', instance.node],
            ['Started', instance.started_at ? relativeTime(instance.started_at) : '-'],
            ['Finished', instance.finished_at ? relativeTime(instance.finished_at) : '-'],
            ['Health', instance.health === 'none' ? 'No health check' : instance.health],
          ]}
        />
      </Panel>

      <StatsPanels id={id} memoryLimit={instance.memory_limit_bytes ?? null} />

      <div className="grid grid--2">
        <ImagePanel instance={instance} update={update} isAdmin={isAdmin} onUpdate={() => run('Update', () => api.updateInstance(id), `Updating ${id}`)} busy={busy === 'Update'} />
        <ServiceActionsPanel id={id} isAdmin={isAdmin} />
      </div>

      <DataPanel requirements={requirements} />
      <LogsPanel id={id} />

      <MoveSheet
        open={moveOpen}
        onClose={() => setMoveOpen(false)}
        instance={instance}
        requirements={requirements}
        onMove={async (node) => {
          setMoveOpen(false);
          await run('Move', () => api.moveInstance(id, node), `Moving ${id} to ${node}`);
        }}
      />
    </div>
  );
}

// ---- metrics ----

function StatsPanels({ id, memoryLimit }: { id: string; memoryLimit: number | null }) {
  const s = useStatsHistory(id);
  const l = s.latest;
  const rate = (v: number[]) => (v.length ? formatBytes(v[v.length - 1]) + '/sample' : '-');
  const memMax = memoryLimit ?? l?.memory_limit_bytes ?? undefined;
  return (
    <div className="grid grid--3">
      <Panel title="CPU">
        <span className="metric">{l ? `${l.cpu_percent.toFixed(1)}%` : '-'}</span>
        <Spark values={s.cpu} tone="ok" label={`CPU over the last ${s.cpu.length} samples`} />
      </Panel>
      <Panel title="Memory">
        <span className="metric">{l ? formatBytes(l.memory_used_bytes) : '-'}</span>
        <span className="sub">{memMax ? `of ${formatBytes(memMax)}` : 'No limit set'}</span>
        <Spark values={s.memory} max={memMax} tone="pop" label={`Memory over the last ${s.memory.length} samples`} />
      </Panel>
      <Panel title="Network">
        <span className="metric">{rate(s.netRx)}</span>
        <span className="sub">{l ? `in ${formatBytes(l.network_rx_bytes)} · out ${formatBytes(l.network_tx_bytes)} total` : 'Waiting for samples'}</span>
        <Spark values={s.netRx} tone="brand" label="Network received per sample" />
      </Panel>
      <Panel title="Disk IO">
        <span className="metric">{rate(s.ioWrite)}</span>
        <span className="sub">{l ? `read ${formatBytes(l.io_read_bytes)} · written ${formatBytes(l.io_write_bytes)} total` : 'Waiting for samples'}</span>
        <Spark values={s.ioWrite} tone="warn" label="Disk written per sample" />
      </Panel>
    </div>
  );
}

// ---- image, actions ----

function ImagePanel({
  instance,
  update,
  isAdmin,
  onUpdate,
  busy,
}: {
  instance: InstanceState;
  update: UpdateRow | null;
  isAdmin: boolean;
  onUpdate: () => void;
  busy: boolean;
}) {
  const running = instance.running_digest ?? update?.running_digest ?? null;
  const registry = update?.latest_digest ?? null;
  return (
    <Panel
      title="Image"
      actions={
        isAdmin && update?.update_available ? (
          <Button size="sm" icon="download" loading={busy} onClick={onUpdate}>
            {`Update ${instance.id}`}
          </Button>
        ) : undefined
      }
    >
      <KV
        items={[
          ['Image', instance.image ?? '-'],
          ['Running', shortDigest(running) || '-'],
          ['Registry', registry ? shortDigest(registry) : '-'],
          ['Pinned', shortDigest(instance.image_digest) || '-'],
          [
            'Proxy check',
            instance.proxy ? (
              instance.proxy.error ? (
                <span className="wl-danger">
                  <Icon name="x" size={14} /> {`Unreachable: ${instance.proxy.error}`}
                </span>
              ) : (
                <StatusBadge status="healthy" label="Reachable" />
              )
            ) : (
              'Nothing published'
            ),
          ],
        ]}
      />
      {update?.update_available && <Tag tone="brand">Update available</Tag>}
    </Panel>
  );
}

function ServiceActionsPanel({ id, isAdmin }: { id: string; isAdmin: boolean }) {
  const [actions, setActions] = useState<{ name: string; confirm: boolean }[]>([]);
  const [result, setResult] = useState<{ name: string; code: number | null; ms?: number; out: string; timedOut: boolean } | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const confirm = useConfirm();
  const toast = useToast();

  useEffect(() => {
    if (!isAdmin) return;
    api.listActions(id).then(setActions).catch(() => setActions([]));
  }, [id, isAdmin]);

  if (!isAdmin || actions.length === 0) return null;

  const run = async (a: { name: string; confirm: boolean }) => {
    if (a.confirm && !(await confirm({ title: `Run ${a.name} on ${id}?`, body: 'This runs inside the container.', confirmLabel: `Run ${a.name}`, confirmVariant: 'danger' }))) return;
    setBusy(a.name);
    try {
      const r = await api.runAction(id, a.name);
      setResult({ name: a.name, code: r.exit_code, ms: r.duration_ms, out: [r.stdout, r.stderr].filter(Boolean).join('\n'), timedOut: r.timed_out });
    } catch (e) {
      toast(`Could not run ${a.name}`, { tone: 'danger', body: e instanceof ApiError ? e.message : undefined });
    } finally {
      setBusy(null);
    }
  };

  return (
    <Panel title="Service actions">
      <div className="acts">
        {actions.map((a) => (
          <Button key={a.name} icon="play" loading={busy === a.name} onClick={() => run(a)}>
            {`Run ${a.name}`}
          </Button>
        ))}
      </div>
      {result && (
        <div className="sec" aria-live="polite">
          <div className="row">
            <strong>{result.name}</strong>
            {result.timedOut ? <StatusBadge status="failed" label="Timed out" /> : result.code === 0 ? <StatusBadge status="healthy" label="Exit 0" /> : <StatusBadge status="failed" label={`Exit ${result.code ?? 'none'}`} />}
            {result.ms != null && <Tag>{formatDuration(result.ms)}</Tag>}
          </div>
          {result.out && <pre className="codeblock">{result.out}</pre>}
        </div>
      )}
    </Panel>
  );
}

// ---- data ----

function DataPanel({ requirements }: { requirements: StackRequirements | null }) {
  const cluster = useClusterState();
  if (!cluster || !requirements || requirements.drives.length === 0) return null;
  // Older daemons don't report `volumes`; fall back to everything on the drives used.
  const own = requirements.volumes ? new Set(requirements.volumes) : null;
  const whole = new Set(requirements.whole_drive_mounts);
  const drives = new Set(requirements.drives);
  const volumes = Object.values(cluster.volumes).filter((v) => {
    const drive = v.id.split('/')[0];
    return own ? own.has(v.id) || whole.has(drive) : drives.has(drive);
  });
  if (volumes.length === 0) return null;
  return (
    <Panel title="Data" flush>
      <p className="sub wl-pad">
        {own
          ? `Volumes this container mounts${whole.size ? `, and everything on ${[...whole].join(', ')}, which it mounts whole` : ''}.`
          : `Volumes on the drives this service uses: ${requirements.drives.join(', ')}.`}
      </p>
      <div className="list list--bare">
        {volumes.map((v) => {
          const sv = volumeStatus(v);
          const behind = Object.entries(v.replicas).filter(([, g]) => g < v.generation).map(([n]) => n);
          return (
            <div className="li" key={v.id}>
              <div className="li__main">
                <span className="li__name mono">{v.id}</span>
                <span className="li__meta">{`Primary ${v.primary} · generation ${v.generation}${behind.length ? ` · behind on ${behind.join(', ')}` : ''}`}</span>
              </div>
              <div className="li__side">
                <StatusBadge status={sv.status} label={sv.label} />
              </div>
            </div>
          );
        })}
      </div>
    </Panel>
  );
}

// ---- logs ----

function LogsPanel({ id }: { id: string }) {
  const { hash } = useLocation();
  const [follow, setFollow] = useState(true);
  const [tail, setTail] = useState(200);
  const [stream, setStream] = useState<'all' | 'stdout' | 'stderr'>('all');
  const [search, setSearch] = useState('');
  const url = `/api/instances/${encodeURIComponent(id)}/logs?follow=${follow}&tail=${tail}`;
  const { items, connected } = useEventSource<StreamLine>(url, logStreamOptions(2000));
  const visible = useMemo(
    () => items.filter((l) => (stream === 'all' || l.stream === stream) && (!search || l.message.toLowerCase().includes(search.toLowerCase()))),
    [items, stream, search],
  );

  useEffect(() => {
    if (hash === '#logs') document.getElementById('container-logs')?.scrollIntoView({ block: 'start' });
  }, [hash]);

  return (
    <Panel
      title="Logs"
      actions={
        <span className="sub wl-inline">
          <Icon name={connected ? 'check-circle' : 'sync'} size={14} />
          {connected ? 'Streaming' : 'Connecting'}
        </span>
      }
    >
      <div id="container-logs" />
      <div className="chips" role="group" aria-label="Log options">
        <Chip label="All output" selected={stream === 'all'} onClick={() => setStream('all')} />
        <Chip label="Output" selected={stream === 'stdout'} onClick={() => setStream('stdout')} />
        <Chip label="Errors" selected={stream === 'stderr'} onClick={() => setStream('stderr')} />
        <Chip label="Last 200" selected={tail === 200} onClick={() => setTail(200)} />
        <Chip label="Last 2000" selected={tail === 2000} onClick={() => setTail(2000)} />
        <Chip label="Follow" icon="sync" selected={follow} onClick={() => setFollow((f) => !f)} />
      </div>
      <TextField label="Search logs" type="search" value={search} onChange={(e) => setSearch(e.target.value)} />
      {visible.length === 0 ? (
        <p className="sub">{items.length === 0 ? 'No log output yet.' : 'No lines match.'}</p>
      ) : (
        <LogView lines={visible.map(toLogViewLine)} label={`Logs for ${id}`} />
      )}
      <Link className="lnk" to="/logs">
        Open the log viewer
      </Link>
    </Panel>
  );
}

// ---- move ----

function MoveSheet({
  open,
  onClose,
  instance,
  requirements,
  onMove,
}: {
  open: boolean;
  onClose: () => void;
  instance: InstanceState;
  requirements: StackRequirements | null;
  onMove: (node: string) => void;
}) {
  const cluster = useClusterState();
  const ctx = usePlacementContext();
  if (!cluster) return null;
  const rows = requirements ? buildPlacementRows(requirements, cluster, ctx) : [];
  const nodes = requirements
    ? nodeEligibility(rows, cluster)
    : Object.keys(cluster.nodes).sort().map((name) => ({
        name,
        eligible: cluster.nodes[name].availability === 'active' && !cluster.unreachable.includes(name),
        reasons: [] as string[],
      }));
  const others = nodes.filter((n) => n.name !== instance.node);
  const eligible = others.filter((n) => n.eligible);
  const blocked = others.filter((n) => !n.eligible);

  return (
    <BottomSheet open={open} title={`Move ${instance.id}`} onClose={onClose}>
      <div className="sec">
        <p className="sub">{`Currently on ${instance.node}. Its data syncs to the new node before the container starts there.`}</p>
        {eligible.length === 0 && <p className="sub">No other node can run this container right now.</p>}
        {eligible.map((n) => (
          <Button key={n.name} icon="move" onClick={() => onMove(n.name)}>
            {`Move to ${n.name}`}
          </Button>
        ))}
        {blocked.length > 0 && (
          <>
            <h3 className="h3">Cannot move here</h3>
            {blocked.map((n) => (
              <div key={n.name} className="li">
                <div className="li__main">
                  <span className="li__name">{n.name}</span>
                  <span className="li__meta wl-danger">
                    <Icon name="x" size={14} /> {n.reasons.join(', ') || 'Not eligible'}
                  </span>
                </div>
              </div>
            ))}
          </>
        )}
      </div>
    </BottomSheet>
  );
}
