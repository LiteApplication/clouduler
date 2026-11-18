import { useCallback, useEffect, useMemo, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { parse } from 'yaml';
import { api, ApiError } from '../../api/client';
import type { TerraformOutputView, TerraformRunMode, TerraformRunView, TerraformServiceStatus, TerraformStatus } from '../../api/types';
import { Banner, Button, EmptyState, KV, Panel, StatusBadge, Tag, useConfirm, useToast } from '../../ds';
import { relativeTime } from '../../lib/format';
import { terraformRunStatus } from '../../lib/status';

const POLL_MS = 3000;

/** Service names declared by the stack spec, in file order. */
function specServices(spec: string): string[] {
  try {
    const doc = parse(spec) as { services?: Record<string, unknown> } | null;
    return doc?.services && typeof doc.services === 'object' ? Object.keys(doc.services) : [];
  } catch {
    return [];
  }
}

function isBusy(s: TerraformServiceStatus | undefined) {
  return !!s && (s.last_run?.outcome === 'running' || s.pending_request != null);
}

export function TerraformTab({ name, spec, isAdmin }: { name: string; spec: string; isAdmin: boolean }) {
  const navigate = useNavigate();
  const [status, setStatus] = useState<TerraformStatus | null | undefined>(undefined);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setStatus(await api.getStackTerraform(name));
      setError(null);
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Could not load the terraform status.');
      setStatus((s) => s ?? null);
    }
  }, [name]);

  useEffect(() => {
    setStatus(undefined);
    void refresh();
  }, [refresh]);

  const busy = !!status?.services.some(isBusy);
  useEffect(() => {
    if (!busy) return;
    const id = setInterval(() => void refresh(), POLL_MS);
    return () => clearInterval(id);
  }, [busy, refresh]);

  const services = useMemo(() => {
    const names = specServices(spec);
    for (const s of status?.services ?? []) if (!names.includes(s.service)) names.push(s.service);
    return names;
  }, [spec, status]);

  if (status === undefined) {
    return (
      <p className="sub" role="status">
        Loading terraform...
      </p>
    );
  }
  if (status === null) {
    return (
      <Banner tone="danger" title="Could not load terraform" action="Retry" onAction={() => void refresh()}>
        {error ?? 'Try reloading.'}
      </Banner>
    );
  }

  return (
    <div className="wl-terraform col">
      {!status.enabled_cluster && (
        <Banner
          tone="warn"
          title="Terraform is disabled in the cluster settings"
          action="Open settings"
          onAction={() => navigate('/settings/general?page=terraform')}
        >
          Modules can still be edited, but plan and apply are unavailable until it is turned on.
        </Banner>
      )}
      {error && (
        <Banner tone="danger" title="Could not refresh">
          {error}
        </Banner>
      )}
      {services.length === 0 && (
        <EmptyState icon="stack" title="No services">
          This stack has no services to attach a module to.
        </EmptyState>
      )}
      {services.map((svc) => (
        <ServiceCard
          key={svc}
          stack={name}
          service={svc}
          status={status.services.find((s) => s.service === svc)}
          clusterEnabled={status.enabled_cluster}
          isAdmin={isAdmin}
          onChanged={refresh}
        />
      ))}
    </div>
  );
}

function ServiceCard({
  stack,
  service,
  status,
  clusterEnabled,
  isAdmin,
  onChanged,
}: {
  stack: string;
  service: string;
  status: TerraformServiceStatus | undefined;
  clusterEnabled: boolean;
  isAdmin: boolean;
  onChanged: () => Promise<void>;
}) {
  const toast = useToast();
  const confirm = useConfirm();
  const hasModule = status != null;
  const [adding, setAdding] = useState(false);
  const [text, setText] = useState(status?.source ?? '');
  const [saved, setSaved] = useState(status?.source ?? '');
  const [working, setWorking] = useState(false);

  // Follow the server copy unless the user has unsaved edits.
  useEffect(() => {
    const src = status?.source ?? '';
    if (text === saved) setText(src);
    setSaved(src);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status?.source, status?.revision]);

  const dirty = text !== saved;
  const failure = (what: string, e: unknown) =>
    toast(`Could not ${what} ${service}`, { tone: 'danger', body: e instanceof ApiError ? e.message : undefined });

  const save = async () => {
    setWorking(true);
    try {
      await api.putTerraformModule(stack, service, text);
      toast(`Saved the module for ${service}`);
      setAdding(false);
      await onChanged();
    } catch (e) {
      failure('save the module of', e);
    } finally {
      setWorking(false);
    }
  };

  const remove = async () => {
    const ok = await confirm({
      title: `Delete the module for ${service}?`,
      body: 'The stored state is forgotten too. Resources the module created are not destroyed.',
      confirmLabel: 'Delete module',
      confirmVariant: 'danger',
    });
    if (!ok) return;
    setWorking(true);
    try {
      await api.deleteTerraformModule(stack, service);
      toast(`Deleted the module for ${service}`);
      setText('');
      setSaved('');
      await onChanged();
    } catch (e) {
      failure('delete the module of', e);
    } finally {
      setWorking(false);
    }
  };

  const run = async (mode: TerraformRunMode) => {
    setWorking(true);
    try {
      await api.runTerraform(stack, service, mode);
      toast(`Requested a terraform ${mode} for ${service}`);
      await onChanged();
    } catch (e) {
      failure(`${mode}`, e);
    } finally {
      setWorking(false);
    }
  };

  const runDisabled = !isAdmin || !clusterEnabled || dirty || working || status?.pending_request != null;
  const runTitle = !clusterEnabled
    ? 'Terraform is disabled in the cluster settings'
    : dirty
      ? 'Save the module first'
      : undefined;

  return (
    <Panel
      title={service}
      actions={
        <>
          {status && !status.enabled && <Tag>Paused</Tag>}
          {status?.pending_request && <Tag>{`${status.pending_request} pending`}</Tag>}
          {hasModule && (
            <>
              <Button size="sm" disabled={runDisabled} title={runTitle} onClick={() => void run('plan')}>
                Plan
              </Button>
              <Button size="sm" variant="primary" disabled={runDisabled} title={runTitle} onClick={() => void run('apply')}>
                Apply
              </Button>
            </>
          )}
        </>
      }
    >
      {!hasModule && !adding ? (
        <div className="wl-stack" style={{ gap: 'var(--space-3)' }}>
          <p className="sub" style={{ margin: 0 }}>No terraform module for this service.</p>
          {isAdmin && (
            <Button size="sm" icon="plus" onClick={() => setAdding(true)}>
              Add module
            </Button>
          )}
        </div>
      ) : (
        <>
          <label className="cl-label" htmlFor={`tf-${service}`}>
            {`Module for ${service} (main.tf)`}
          </label>
          <textarea
            id={`tf-${service}`}
            className="mono"
            style={{ width: '100%', minHeight: 220, fontFamily: 'var(--mono, monospace)' }}
            spellCheck={false}
            value={text}
            readOnly={!isAdmin}
            onChange={(e) => setText(e.target.value)}
          />
          {isAdmin && (
            <div className="btn-row" style={{ marginTop: 8 }}>
              <Button variant="primary" size="sm" disabled={working || !dirty} onClick={() => void save()}>
                Save
              </Button>
              {hasModule ? (
                <Button variant="danger" size="sm" icon="trash" disabled={working} onClick={() => void remove()}>
                  Delete
                </Button>
              ) : (
                <Button size="sm" onClick={() => setAdding(false)}>
                  Cancel
                </Button>
              )}
              {dirty && <span className="sub">Unsaved changes</span>}
            </div>
          )}
          {status && status.outputs.length > 0 && <WrittenObjects outputs={status.outputs} />}
          {status && <RunSummary run={status.last_run} revision={status.revision} />}
        </>
      )}
    </Panel>
  );
}

/** The cluster secrets and configs this module's outputs wrote (names and revisions only). */
function WrittenObjects({ outputs }: { outputs: TerraformOutputView[] }) {
  return (
    <div style={{ marginTop: 12 }}>
      <span className="sub">Writes</span>
      <ul style={{ margin: '4px 0 0', paddingLeft: 20 }}>
        {outputs.map((o) => (
          <li key={`${o.kind}/${o.name}`}>
            {o.kind} <span className="mono">{o.name}</span> <span className="sub">{`(rev ${o.revision})`}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function RunSummary({ run, revision }: { run: TerraformRunView | null; revision: number }) {
  if (!run) {
    return <p className="sub">{`Revision ${revision}. No run yet.`}</p>;
  }
  const st = terraformRunStatus(run.outcome);
  return (
    <div style={{ marginTop: 12 }}>
      <div className="btn-row">
        <StatusBadge status={st.status} label={st.label} />
        <span>{run.mode === 'plan' ? 'Plan' : 'Apply'}</span>
        <Tag>{run.trigger}</Tag>
      </div>
      <KV
        items={[
          ['Revision', String(revision)],
          ['Node', run.node],
          ['Started', relativeTime(run.started_at)],
          ['Finished', run.finished_at ? relativeTime(run.finished_at) : '-'],
        ]}
      />
      {run.message && <p className="sub">{run.message}</p>}
      {run.log_tail && (
        <details>
          <summary>Log</summary>
          <pre className="mono" style={{ overflow: 'auto', maxHeight: 320 }}>
            {run.log_tail}
          </pre>
        </details>
      )}
    </div>
  );
}
