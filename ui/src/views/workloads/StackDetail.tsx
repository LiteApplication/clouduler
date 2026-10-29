import { useEffect, useMemo, useState } from 'react';
import { Link, useNavigate, useParams, useSearchParams } from 'react-router-dom';
import { api, ApiError } from '../../api/client';
import type { Assignment, InstanceState, StackDetailResponse, StackValidationResult, UpdateRow } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import {
  Banner,
  Button,
  ContainerRow,
  EmptyState,
  Icon,
  KV,
  LogView,
  PageHeader,
  Panel,
  StatusBadge,
  StepBar,
  Tabs,
  Tag,
  TextField,
  useConfirm,
  useToast,
} from '../../ds';
import { useEventSource } from '../../hooks/useEventSource';
import { relativeTime, formatDuration } from '../../lib/format';
import { instanceStatus } from '../../lib/status';
import { diffLines } from '../../lib/specText';
import { useInstanceProgress } from '../../state/progressStore';
import { useInstances } from '../../state/instancesStore';
import { useClusterState } from '../../state/store';
import { StackSpecEditor } from '../../components/StackSpecEditor';
import { DnsCertPanel, PlacementPanel } from './PlacementPanel';
import { logStreamOptions, toLogViewLine, type StreamLine } from './logLines';
import { copyText, useKnownNames } from './specTools';
import './workloads.css';

export function StackDetail() {
  const { name = '' } = useParams();
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const navigate = useNavigate();
  const toast = useToast();
  const [params, setParams] = useSearchParams();
  const tab = (['containers', 'placement', 'spec', 'settings'] as const).find((t) => t === params.get('tab')) ?? 'containers';
  const stack = cluster?.stacks[name];
  const known = useKnownNames();
  const polled = useInstances();
  const [updates, setUpdates] = useState<UpdateRow[]>([]);

  useEffect(() => {
    let cancelled = false;
    api
      .listUpdates?.()
      ?.then((rows) => !cancelled && setUpdates(rows.filter((r) => r.stack === name)))
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [name]);

  const assignments = useMemo(
    () => (cluster ? Object.values(cluster.assignments).filter((a) => a.instance.startsWith(`${name}.`)) : []),
    [cluster, name],
  );
  const live = useMemo(() => {
    const byId: Record<string, InstanceState> = {};
    for (const i of polled ?? []) if (i.id.startsWith(`${name}.`)) byId[i.id] = i;
    return byId;
  }, [polled, name]);

  if (!stack) {
    if (!cluster) {
      return (
        <p className="sub" role="status">
          Loading stack...
        </p>
      );
    }
    return (
      <EmptyState
        icon="stack"
        title="No stack called that"
        action={
          <Button variant="primary" onClick={() => navigate('/stacks')}>
            Back to stacks
          </Button>
        }
      >
        {`There is no stack named ${name}. It may have been removed.`}
      </EmptyState>
    );
  }

  const nodes = new Set(assignments.map((a) => a.node));
  const services = new Set(assignments.map((a) => a.instance.split('.')[1] ?? a.instance));
  const failing = assignments.filter((a) => hasIssue(a, live[a.instance])).length;
  const outdated = updates.filter((r) => r.update_available);

  const copySpec = async () =>
    (await copyText(stack.spec))
      ? toast('Spec copied')
      : toast('Could not copy the spec', { tone: 'danger', body: 'Your browser blocked clipboard access. Select the spec and copy it by hand.' });

  const setTab = (id: string) => {
    const next = new URLSearchParams(params);
    if (id === 'containers') next.delete('tab');
    else next.set('tab', id);
    setParams(next, { replace: true });
  };

  return (
    <div className="sec wl-page">
      <PageHeader
        breadcrumb={[{ label: 'Stacks', to: '/stacks' }, { label: name }]}
        title={name}
        sub={[
          `Revision ${stack.revision}`,
          `${services.size} ${services.size === 1 ? 'service' : 'services'}`,
          `${nodes.size} ${nodes.size === 1 ? 'node' : 'nodes'}`,
          stack.deployed_at ? `Deployed ${relativeTime(stack.deployed_at)}` : null,
        ]
          .filter(Boolean)
          .join(' · ')}
        actions={
          <>
            <Button icon="copy" onClick={copySpec}>
              Copy spec
            </Button>
            {isAdmin && (
              <Button variant="primary" icon="upload" onClick={() => navigate(`/stacks/${encodeURIComponent(name)}/deploy`)}>
                {`Redeploy ${name}`}
              </Button>
            )}
          </>
        }
      />

      {outdated.length > 0 && (
        <Banner
          tone="info"
          title={`${outdated.length} ${outdated.length === 1 ? 'container has' : 'containers have'} an image update`}
          action="Review updates"
          onAction={() => navigate('/updates')}
        >
          {outdated.map((r) => r.id).join(', ')}
        </Banner>
      )}
      {failing > 0 && (
        <Banner tone="danger" title={`${failing} of ${assignments.length} containers need attention`}>
          Open the Containers tab for the recent logs of each one.
        </Banner>
      )}

      <Tabs
        label="Stack sections"
        active={tab}
        onChange={setTab}
        items={[
          { id: 'containers', label: 'Containers', count: assignments.length },
          { id: 'placement', label: 'Placement' },
          { id: 'spec', label: 'Spec' },
          { id: 'settings', label: 'Settings' },
        ]}
      />

      {tab === 'containers' && <ContainersTab name={name} assignments={assignments} live={live} isAdmin={isAdmin} />}
      {tab === 'placement' && <PlacementTab name={name} spec={stack.spec} />}
      {tab === 'spec' && <SpecTab spec={stack.spec} previous={stack.previous_spec ?? null} known={known} />}
      {tab === 'settings' && (
        <SettingsTab
          name={name}
          isAdmin={isAdmin}
          configs={stack.used_configs}
          secrets={stack.used_secrets}
          onRemoved={() => navigate('/stacks')}
        />
      )}
    </div>
  );
}

function hasIssue(a: Assignment, i: InstanceState | undefined) {
  if (!i) return false;
  return (a.desired === 'running' && i.state !== 'running') || i.health === 'unhealthy';
}

// ---- containers ----

function ContainersTab({
  name,
  assignments,
  live,
  isAdmin,
}: {
  name: string;
  assignments: Assignment[];
  live: Record<string, InstanceState>;
  isAdmin: boolean;
}) {
  const unplaced = Object.values(live).filter((i) => i.state === 'pending_placement' && !assignments.some((a) => a.instance === i.id));
  if (assignments.length === 0 && unplaced.length === 0) {
    return (
      <EmptyState icon="container" title="Nothing running yet">
        {`${name} has no containers. If you just deployed it, give the scheduler a moment.`}
      </EmptyState>
    );
  }
  return (
    <div className="sec">
      <div className="wl-rows">
        {assignments.map((a) => (
          <InstanceRow key={a.instance} a={a} i={live[a.instance]} isAdmin={isAdmin} />
        ))}
        {unplaced.map((i) => (
          <InstanceRow key={i.id} a={{ instance: i.id, node: '', desired: 'running', image_digest: null, migration: null }} i={i} isAdmin={isAdmin} />
        ))}
      </div>
      <ServiceActions instances={assignments.map((a) => a.instance)} isAdmin={isAdmin} />
    </div>
  );
}

function InstanceRow({ a, i, isAdmin }: { a: Assignment; i: InstanceState | undefined; isAdmin: boolean }) {
  const navigate = useNavigate();
  const toast = useToast();
  const progress = useInstanceProgress(a.instance);
  const view = i ? instanceStatus(i) : { status: 'stopped' as const, label: 'Unknown' };
  const issue = hasIssue(a, i);
  const migration = i?.migration ?? a.migration;
  const phase = migration && typeof migration.phase === 'string' ? migration.phase : null;
  const failedPhase = migration && typeof migration.phase !== 'string' ? migration.phase.failed.reason : null;
  const STEPS = ['stopping', 'syncing', 'verifying', 'switching', 'starting'];

  const act = async (what: 'restart' | 'start') => {
    try {
      if (what === 'restart') await api.restartInstance(a.instance);
      else await api.setDesired(a.instance, 'running');
      toast(what === 'restart' ? `Restarting ${a.instance}` : `Starting ${a.instance}`);
    } catch (e) {
      toast(`Could not ${what} ${a.instance}`, { tone: 'danger', body: e instanceof ApiError ? e.message : undefined });
    }
  };

  return (
    <div>
      <ContainerRow
        name={a.instance}
        image={i?.image}
        node={a.node || undefined}
        ports={i?.restart_count ? `restarted ${i.restart_count}×` : undefined}
        status={view.status}
        onOpen={() => navigate(`/instances/${encodeURIComponent(a.instance)}`)}
        onLogs={() => navigate(`/instances/${encodeURIComponent(a.instance)}#logs`)}
        onRestart={isAdmin ? () => act('restart') : undefined}
        onStart={isAdmin ? () => act('start') : undefined}
        extra={
          <>
            {i?.placement_error && (
              <span className="sub wl-danger">
                <Icon name="x" size={14} /> {`No node can run this: ${i.placement_error}`}
              </span>
            )}
            {progress?.reason && (
              <span className="sub">
                {progress.step && <StepBar step={progress.step[0]} total={progress.step[1]} />} {progress.reason}
              </span>
            )}
            {phase && phase !== 'done' && (
              <span className="sub wl-inline">
                <StepBar step={STEPS.indexOf(phase) + 1} total={STEPS.length} label={`Move ${phase}`} />
                {`Moving to ${migration!.to}: ${phase}`}
              </span>
            )}
            {failedPhase && (
              <span className="sub wl-danger">
                <Icon name="x" size={14} /> {`Move to ${migration!.to} failed: ${failedPhase}`}
              </span>
            )}
          </>
        }
      />
      {issue && i && (
        <div className="wl-issue">
          <p className="sub wl-danger">
            <Icon name="x" size={14} />
            {` ${a.instance} should be ${a.desired} but is ${i.state}${i.health === 'unhealthy' ? ' and unhealthy' : ''}${a.node ? ` on ${a.node}` : ''}. Recent logs:`}
          </p>
          <InlineLogs id={a.instance} />
        </div>
      )}
    </div>
  );
}

function InlineLogs({ id }: { id: string }) {
  const url = `/api/instances/${encodeURIComponent(id)}/logs?follow=false&tail=30`;
  const { items, connected } = useEventSource<StreamLine>(url, logStreamOptions(30));
  return (
    <div className="sec">
      {items.length === 0 ? (
        <p className="sub">{connected ? 'No log output yet.' : 'Connecting to the log stream...'}</p>
      ) : (
        <LogView lines={items.map(toLogViewLine)} wrap label={`Recent logs for ${id}`} />
      )}
      <Link className="lnk" to={`/instances/${encodeURIComponent(id)}#logs`}>
        View all logs for {id}
      </Link>
    </div>
  );
}

/** Service actions are declared per service (`x-clouduler.actions`); show them
 *  once per service, run against its first instance. */
function ServiceActions({ instances, isAdmin }: { instances: string[]; isAdmin: boolean }) {
  const [actions, setActions] = useState<{ id: string; name: string; confirm: boolean }[]>([]);
  const [results, setResults] = useState<Record<string, { code: number | null; ms?: number; out: string; timedOut: boolean }>>({});
  const [busy, setBusy] = useState<string | null>(null);
  const confirm = useConfirm();
  const toast = useToast();
  const key = instances.join(',');

  useEffect(() => {
    let cancelled = false;
    (async () => {
      const seen = new Set<string>();
      const found: { id: string; name: string; confirm: boolean }[] = [];
      for (const id of instances) {
        const service = id.split('.')[1];
        if (seen.has(service)) continue;
        seen.add(service);
        try {
          for (const act of await api.listActions(id)) found.push({ id, ...act });
        } catch {
          /* a service with no actions answers an error on some versions */
        }
      }
      if (!cancelled) setActions(found);
    })();
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  if (!isAdmin || actions.length === 0) return null;

  const run = async (a: { id: string; name: string; confirm: boolean }) => {
    if (a.confirm && !(await confirm({ title: `Run ${a.name} on ${a.id}?`, body: 'This runs inside the container.', confirmLabel: `Run ${a.name}`, confirmVariant: 'danger' }))) return;
    const k = `${a.id}/${a.name}`;
    setBusy(k);
    try {
      const r = await api.runAction(a.id, a.name);
      setResults((cur) => ({
        ...cur,
        [k]: { code: r.exit_code, ms: r.duration_ms, out: [r.stdout, r.stderr].filter(Boolean).join('\n'), timedOut: r.timed_out },
      }));
    } catch (e) {
      toast(`Could not run ${a.name}`, { tone: 'danger', body: e instanceof ApiError ? e.message : undefined });
    } finally {
      setBusy(null);
    }
  };

  return (
    <Panel title="Service actions">
      <div className="sec">
        {actions.map((a) => {
          const k = `${a.id}/${a.name}`;
          const r = results[k];
          return (
            <div key={k} className="sec">
              <div className="row row--between">
                <span>
                  <strong>{a.name}</strong> <span className="sub mono">{a.id}</span>
                </span>
                <Button icon="play" loading={busy === k} onClick={() => run(a)}>
                  {`Run ${a.name}`}
                </Button>
              </div>
              {r && (
                <>
                  <div className="row">
                    {r.timedOut ? (
                      <StatusBadge status="failed" label="Timed out" />
                    ) : r.code === 0 ? (
                      <StatusBadge status="healthy" label="Exit 0" />
                    ) : (
                      <StatusBadge status="failed" label={`Exit ${r.code ?? 'none'}`} />
                    )}
                    {r.ms != null && <Tag>{formatDuration(r.ms)}</Tag>}
                  </div>
                  {r.out && <pre className="codeblock">{r.out}</pre>}
                </>
              )}
            </div>
          );
        })}
      </div>
    </Panel>
  );
}

// ---- placement ----

function PlacementTab({ name, spec }: { name: string; spec: string }) {
  const [result, setResult] = useState<StackValidationResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    api
      .validateStack(name, spec)
      .then((r) => !cancelled && setResult(r))
      .catch((e) => !cancelled && setError(e instanceof ApiError ? e.message : 'The placement could not be worked out. Try reloading.'));
    return () => {
      cancelled = true;
    };
  }, [name, spec]);

  if (error) return <Banner tone="danger" title="Placement unavailable">{error}</Banner>;
  if (!result) return <p className="sub" role="status">Working out placement...</p>;
  return (
    <div className="sec">
      <PlacementPanel perService={result.per_service} />
      <DnsCertPanel hosts={result.requirements?.hosts ?? []} />
    </div>
  );
}

// ---- spec ----

function SpecTab({ spec, previous, known }: { spec: string; previous: string | null; known: ReturnType<typeof useKnownNames> }) {
  const toast = useToast();
  const diff = useMemo(() => (previous != null && previous !== spec ? diffLines(previous, spec) : []), [previous, spec]);
  return (
    <div className="sec">
      <div className="acts">
        <Button
          icon="copy"
          onClick={async () => ((await copyText(spec)) ? toast('Spec copied') : toast('Could not copy the spec', { tone: 'danger' }))}
        >
          Copy spec
        </Button>
      </div>
      <StackSpecEditor value={spec} editable={false} known={known} minHeight="240px" />
      {diff.length > 0 && (
        <Panel title="Changes since last deploy">
          <DiffView lines={diff} />
        </Panel>
      )}
    </div>
  );
}

export function DiffView({ lines }: { lines: ReturnType<typeof diffLines> }) {
  return (
    <pre className="diff" aria-label="Spec changes">
      {lines.map((l, i) => (
        <div key={i} className={`diff__l diff__l--${l.kind}`}>
          <span aria-hidden="true">{l.kind === 'add' ? '+' : l.kind === 'del' ? '-' : ' '}</span>
          <span className="sr-only">{l.kind === 'add' ? 'Added: ' : l.kind === 'del' ? 'Removed: ' : ''}</span>
          <span>{l.text || ' '}</span>
        </div>
      ))}
    </pre>
  );
}

// ---- settings ----

/** Every network the stack uses, plus any it only appears in `allowed` for. */
function ReachablePanel({ requirements }: { requirements: StackDetailResponse['requirements'] }) {
  const allowed = requirements.allowed ?? {};
  const networks = [...new Set([...requirements.networks, ...Object.keys(allowed)])];
  return (
    <Panel title="Reachable hosts per network">
      <p className="sub">Hosts outside this stack its containers may reach. Traffic inside the stack is always allowed.</p>
      {networks.length === 0 ? (
        <p className="cl-sm cl-muted">This stack uses no networks.</p>
      ) : (
        <KV
          items={networks.map((net) => {
            const hosts = allowed[net];
            return [
              net,
              hosts == null ? (
                <Tag tone="warn">Unrestricted</Tag>
              ) : hosts.length === 0 ? (
                'Nothing outside the stack'
              ) : (
                hosts.map((h) => (
                  <Tag key={h}>
                    <span className="mono">{h}</span>
                  </Tag>
                ))
              ),
            ];
          })}
        />
      )}
    </Panel>
  );
}

function SettingsTab({
  name,
  isAdmin,
  configs,
  secrets,
  onRemoved,
}: {
  name: string;
  isAdmin: boolean;
  configs: string[];
  secrets: string[];
  onRemoved: () => void;
}) {
  const confirm = useConfirm();
  const toast = useToast();
  // undefined while loading, null when the fetch failed.
  const [requirements, setRequirements] = useState<StackDetailResponse['requirements'] | null | undefined>(undefined);

  useEffect(() => {
    let cancelled = false;
    setRequirements(undefined);
    api
      .getStack(name)
      .then((s) => !cancelled && setRequirements(s.requirements))
      .catch(() => !cancelled && setRequirements(null));
    return () => {
      cancelled = true;
    };
  }, [name]);

  const remove = async () => {
    const ok = await confirm({
      title: `Remove ${name}?`,
      body: 'This stops and drops every container in the stack. Its volumes are kept.',
      confirmLabel: `Remove ${name}`,
      confirmVariant: 'danger',
      requireText: name,
    });
    if (!ok) return;
    try {
      await api.removeStack(name);
      toast(`Removed ${name}`);
      onRemoved();
    } catch (e) {
      toast(`Could not remove ${name}`, { tone: 'danger', body: e instanceof ApiError ? e.message : undefined });
    }
  };

  return (
    <div className="sec">
      {requirements === null && <p className="cl-sm cl-muted">Could not load the network rules.</p>}
      {requirements && <ReachablePanel requirements={requirements} />}
      <Panel title="Configs and secrets used">
        <KV
          items={[
            ['Configs', configs.length ? configs.map((c) => <Tag key={c}>{c}</Tag>) : 'None'],
            ['Secrets', secrets.length ? secrets.map((s) => <Tag key={s}>{s}</Tag>) : 'None'],
          ]}
        />
      </Panel>
      {isAdmin && <UpdateWebhook name={name} />}
      {isAdmin && (
        <Panel title="Remove stack">
          <p className="sub">Stops every container of {name} and deletes the stack. You will be asked to type its name first.</p>
          <div className="acts">
            <Button variant="danger" icon="trash" onClick={remove}>
              {`Remove ${name}`}
            </Button>
          </div>
        </Panel>
      )}
    </div>
  );
}

function UpdateWebhook({ name }: { name: string }) {
  const [path, setPath] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const confirm = useConfirm();
  const toast = useToast();

  useEffect(() => {
    api
      .stackUpdateWebhook(name)
      .then((r) => setPath(r.path))
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
  }, [name]);

  if (error) return <Banner tone="warn" title="Update webhook unavailable">{error}</Banner>;
  if (!path) return null;
  const url = `${window.location.origin}${path}`;

  const regenerate = async () => {
    if (!(await confirm({ title: 'Regenerate the webhook URL?', body: 'The current URL stops working immediately.', confirmLabel: 'Regenerate URL', confirmVariant: 'danger' }))) return;
    try {
      setPath((await api.regenerateStackUpdateWebhook(name)).path);
      toast('Webhook URL regenerated');
    } catch (e) {
      toast('Could not regenerate the URL', { tone: 'danger', body: e instanceof Error ? e.message : undefined });
    }
  };

  return (
    <Panel
      title="Update webhook"
      actions={
        <>
          <Button size="sm" icon="copy" onClick={async () => ((await copyText(url)) ? toast('URL copied') : toast('Could not copy the URL', { tone: 'danger' }))}>
            Copy URL
          </Button>
          <Button size="sm" icon="refresh" onClick={regenerate}>
            Regenerate URL
          </Button>
        </>
      }
    >
      <p className="sub">
        POST to this URL from a CI job to pull and restart every container of this stack. Anyone holding the URL can trigger an update, so keep it secret and regenerate it if it leaks.
      </p>
      <TextField label="Webhook URL" mono readOnly value={url} />
      <pre className="codeblock">{`curl -fsS -X POST '${url}'`}</pre>
    </Panel>
  );
}
