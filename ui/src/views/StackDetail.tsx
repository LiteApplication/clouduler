import { Fragment, useEffect, useMemo, useRef, useState } from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { api, ApiError, decodeText, encodeText } from '../api/client';
import type { Assignment, InstanceState, LogLine, StackRequirements, StackState, ValidationError } from '../api/types';
import { useClusterState } from '../state/store';
import { useInstanceProgress } from '../state/progressStore';
import { useAuth } from '../auth/AuthContext';
import { Badge } from '../components/Badge';
import { ConstraintTable } from '../components/ConstraintTable';
import { Icon } from '../components/Icon';
import { StackSpecEditor } from '../components/StackSpecEditor';
import { useEventSource } from '../hooks/useEventSource';
import { instanceStateTone } from '../lib/format';
import { collectImages, emptyKnownNames, type KnownNames } from '../lib/stackSchema';
import { SecretForm } from './SecretForm';
import { statusBadge } from './Certificates';
import { LogMessage } from '../lib/ansi';

/** Everything this cluster already knows the name of, for the spec editor's
 *  live-data autocomplete and ctrl+click navigation — all of it already
 *  present in `ClusterState`, so no extra api calls are needed. */
function useKnownNames(): KnownNames {
  const cluster = useClusterState();
  return useMemo(() => {
    if (!cluster) return emptyKnownNames;
    const networks = new Set<string>();
    const drives = new Set<string>();
    for (const node of Object.values(cluster.nodes)) {
      for (const n of node.networks) networks.add(n);
      for (const d of Object.keys(node.drives)) drives.add(d);
    }
    return {
      networks: Array.from(networks).sort(),
      drives: Array.from(drives).sort(),
      nodes: Object.keys(cluster.nodes).sort(),
      instances: Object.keys(cluster.assignments),
      secrets: Object.keys(cluster.secrets).sort(),
      configs: Object.keys(cluster.configs).sort(),
      volumes: Object.keys(cluster.volumes).sort(),
      images: collectImages(Object.values(cluster.stacks).map((s) => s.spec)),
    };
  }, [cluster]);
}

/** `configs` (service-referenced) and `file_configs` (declared with a bundled
 *  `file:`) aren't subsets of one another — a declared-but-unreferenced entry
 *  can be file-only, and a referenced entry can have no `file:` at all — so
 *  the editable set is their union. */
function allConfigKeys(requirements: StackRequirements): string[] {
  return Array.from(new Set([...requirements.configs, ...requirements.file_configs])).sort();
}

function allSecretKeys(requirements: StackRequirements): string[] {
  return Array.from(new Set([...requirements.secrets, ...requirements.file_secrets])).sort();
}

function InstanceStateCell({
  instance,
  state,
  placementError,
}: {
  instance: string;
  state: InstanceState['state'];
  placementError?: string;
}) {
  const progress = useInstanceProgress(instance);
  return (
    <>
      <Badge tone={instanceStateTone(state)} title={placementError}>
        {state}
      </Badge>
      {progress?.reason && <div className="page-sub">{progress.reason}</div>}
    </>
  );
}

export function StackDetail() {
  const { name = '' } = useParams();
  const isNew = name === '_new';
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const navigate = useNavigate();

  const stack = !isNew ? cluster?.stacks[name] : undefined;
  const known = useKnownNames();
  const [editing, setEditing] = useState(isNew);
  const [nameInput, setNameInput] = useState(isNew ? '' : name);
  const [spec, setSpec] = useState(stack?.spec ?? '');
  const [errors, setErrors] = useState<ValidationError[]>([]);
  const [requirements, setRequirements] = useState<StackRequirements | null>(null);
  const [configDrafts, setConfigDrafts] = useState<Record<string, string>>({});
  const [secretDrafts, setSecretDrafts] = useState<Record<string, string>>({});
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [jumpToConfigFiles, setJumpToConfigFiles] = useState(false);
  const configFilesRef = useRef<HTMLDivElement>(null);

  const openEdit = () => {
    setNameInput(name);
    setSpec(stack?.spec ?? '');
    setErrors([]);
    setRequirements(null);
    setConfigDrafts({});
    setSecretDrafts({});
    setEditing(true);
  };

  // "Config files" jumps straight into the editor's config/secret section
  // instead of making the admin click Validate first to reveal it: it
  // validates the current spec itself, using the stack's own spec rather
  // than the `spec` state (which openEdit only schedules an update for, and
  // so isn't visible to a validate() called in the same tick).
  const openConfigFiles = async () => {
    setNameInput(name);
    setSpec(stack?.spec ?? '');
    setErrors([]);
    setConfigDrafts({});
    setSecretDrafts({});
    setEditing(true);
    setJumpToConfigFiles(true);
    setSubmitError(null);
    try {
      const result = await api.validateStack(name, stack?.spec ?? '');
      setErrors(result.errors);
      setRequirements(result.requirements);
    } catch (e) {
      setSubmitError(e instanceof ApiError ? e.message : 'validation failed');
    }
  };

  useEffect(() => {
    if (!jumpToConfigFiles || !requirements) return;
    configFilesRef.current?.scrollIntoView({ behavior: 'smooth', block: 'start' });
    setJumpToConfigFiles(false);
  }, [jumpToConfigFiles, requirements]);

  const onFile = async (file: File) => {
    setSpec(await file.text());
  };

  // Seed a draft field for every config/secret the spec references, whether
  // or not it bundles its own `file:` — a config the stack only refers to by
  // name still needs somewhere to edit its content, without clobbering
  // anything already typed in.
  useEffect(() => {
    if (!requirements) return;
    setConfigDrafts((prev) => {
      const next = { ...prev };
      for (const key of allConfigKeys(requirements)) {
        if (!(key in next)) {
          const existing = cluster?.configs[key];
          next[key] = existing ? decodeText(existing.content) : '';
        }
      }
      return next;
    });
    setSecretDrafts((prev) => {
      const next = { ...prev };
      for (const key of allSecretKeys(requirements)) {
        if (!(key in next)) next[key] = '';
      }
      return next;
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [requirements]);

  const validate = async () => {
    setSubmitError(null);
    try {
      const result = await api.validateStack(nameInput || 'preview', spec);
      setErrors(result.errors);
      setRequirements(result.requirements);
      return result.valid;
    } catch (e) {
      setSubmitError(e instanceof ApiError ? e.message : 'validation failed');
      return false;
    }
  };

  const deploy = async () => {
    if (!nameInput.trim()) {
      setSubmitError('a stack name is required');
      return;
    }
    setBusy(true);
    setSubmitError(null);
    let step = 'validating spec';
    try {
      const result = await api.validateStack(nameInput, spec);
      setErrors(result.errors);
      setRequirements(result.requirements);
      if (!result.valid || !result.requirements) return;

      step = 'encoding config content';
      const configs: Record<string, string> = {};
      for (const key of result.requirements.file_configs) {
        const draft = configDrafts[key];
        if (draft) {
          configs[key] = encodeText(draft);
        } else if (!cluster?.configs[key]) {
          setSubmitError(`config "${key}" needs content before this stack can deploy`);
          return;
        }
      }
      step = 'encoding secret content';
      const secrets: Record<string, string> = {};
      for (const key of result.requirements.file_secrets) {
        const draft = secretDrafts[key];
        if (draft) {
          secrets[key] = encodeText(draft);
        } else if (!cluster?.secrets[key]) {
          setSubmitError(`secret "${key}" needs content before this stack can deploy`);
          return;
        }
      }

      // Configs/secrets the spec only refers to by name (no `file:`) aren't
      // bundled with the deploy request — the server ignores keys outside
      // file_configs/file_secrets there — so any edited content for them is
      // saved directly as a standalone object instead.
      step = 'saving standalone config content';
      for (const key of result.requirements.configs) {
        if (result.requirements.file_configs.includes(key)) continue;
        const draft = configDrafts[key];
        if (draft) await api.putConfig(key, encodeText(draft));
      }
      step = 'saving standalone secret content';
      for (const key of result.requirements.secrets) {
        if (result.requirements.file_secrets.includes(key)) continue;
        const draft = secretDrafts[key];
        if (draft) await api.putSecret(key, { content: encodeText(draft) });
      }

      step = 'submitting deploy';
      await api.deployStack(nameInput.trim(), { spec, configs, secrets });
      navigate(`/stacks/${encodeURIComponent(nameInput.trim())}`);
      setEditing(false);
    } catch (e) {
      const message = e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e);
      console.error(`[deploy] ${nameInput || '(unnamed)'} failed while ${step}:`, e);
      setSubmitError(`deploy failed while ${step}: ${message}`);
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    if (!confirm(`Remove stack ${name}? This drops all its instances.`)) return;
    try {
      await api.removeStack(name);
      navigate('/stacks');
    } catch (e) {
      alert(e instanceof ApiError ? e.message : 'failed to remove stack');
    }
  };

  if (!isNew && !stack && cluster) {
    return (
      <p className="muted flex-row">
        <Icon name="alert-circle" /> No such stack.
      </p>
    );
  }

  if (editing) {
    return (
      <div>
        <div className="page-header">
          <div className="page-title">
            <Icon name="stacks" />
            <h1>{isNew ? 'Deploy stack' : `Redeploy ${name}`}</h1>
          </div>
        </div>
        <div className="field">
          <label htmlFor="stack-name">Name</label>
          <input
            id="stack-name"
            type="text"
            value={nameInput}
            onChange={(e) => setNameInput(e.target.value)}
            disabled={!isNew}
          />
        </div>
        <div className="field">
          <label htmlFor="stack-spec">Spec (yaml)</label>
          <input
            type="file"
            accept=".yml,.yaml"
            onChange={(e) => e.target.files?.[0] && onFile(e.target.files[0])}
          />
          <StackSpecEditor value={spec} onChange={setSpec} editable known={known} minHeight="360px" />
        </div>
        {errors.length > 0 && (
          <div className="banner banner-bad" role="alert">
            <Icon name="alert" />
            <ul style={{ margin: 0, paddingLeft: 18 }}>
              {errors.map((err, i) => (
                <li key={i}>
                  {err.path && <span className="mono">{err.path}: </span>}
                  {err.message}
                </li>
              ))}
            </ul>
          </div>
        )}

        {requirements && (allConfigKeys(requirements).length > 0 || allSecretKeys(requirements).length > 0) && (
          <div className="section" ref={configFilesRef}>
            <div className="section-head">
              <h2>
                <Icon name="file" /> Config &amp; secret files
              </h2>
            </div>
            <p className="page-sub">
              These are every config/secret this spec references, whether bundled with a <code>file:</code> or an
              existing standalone object. Leave a field blank to keep its current value.
            </p>
            {allConfigKeys(requirements).map((key) => (
              <div className="field" key={`config-${key}`}>
                <label htmlFor={`config-${key}`}>
                  config: {key} {!cluster?.configs[key] && <Badge tone="warn">new</Badge>}
                  {!requirements.file_configs.includes(key) && <Badge tone="neutral">standalone</Badge>}
                </label>
                <input
                  type="file"
                  onChange={(e) =>
                    e.target.files?.[0]?.text().then((text) => setConfigDrafts((prev) => ({ ...prev, [key]: text })))
                  }
                />
                <textarea
                  id={`config-${key}`}
                  rows={6}
                  value={configDrafts[key] ?? ''}
                  onChange={(e) => setConfigDrafts((prev) => ({ ...prev, [key]: e.target.value }))}
                  placeholder={cluster?.configs[key] ? 'leave blank to keep the current content' : 'file content'}
                />
              </div>
            ))}
            {allSecretKeys(requirements).map((key) => (
              <div className="field" key={`secret-${key}`}>
                <label htmlFor={`secret-${key}`}>
                  secret: {key} {!cluster?.secrets[key] && <Badge tone="warn">new</Badge>}
                  {!requirements.file_secrets.includes(key) && <Badge tone="neutral">standalone</Badge>}
                </label>
                <input
                  type="file"
                  onChange={(e) =>
                    e.target.files?.[0]?.text().then((text) => setSecretDrafts((prev) => ({ ...prev, [key]: text })))
                  }
                />
                <textarea
                  id={`secret-${key}`}
                  rows={4}
                  value={secretDrafts[key] ?? ''}
                  onChange={(e) => setSecretDrafts((prev) => ({ ...prev, [key]: e.target.value }))}
                  placeholder={cluster?.secrets[key] ? 'leave blank to keep the current value' : 'secret value'}
                />
              </div>
            ))}
          </div>
        )}

        {submitError && (
          <p className="field-error">
            <Icon name="alert-circle" /> {submitError}
          </p>
        )}
        <div className="btn-row">
          <button onClick={validate} disabled={busy}>
            <Icon name="check" /> Validate
          </button>
          <button className="primary" onClick={deploy} disabled={busy}>
            <Icon name="upload" /> {isNew ? 'Deploy' : 'Redeploy'}
          </button>
          <button onClick={() => (isNew ? navigate('/stacks') : setEditing(false))} disabled={busy}>
            <Icon name="close" /> Cancel
          </button>
        </div>
      </div>
    );
  }

  if (!stack) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading stack...
      </p>
    );
  }

  const instances = cluster
    ? Object.values(cluster.assignments).filter((a) => a.instance.startsWith(`${name}.`))
    : [];

  return (
    <StackDetailBody
      name={name}
      stack={stack}
      instances={instances}
      isAdmin={isAdmin}
      known={known}
      openEdit={openEdit}
      openConfigFiles={openConfigFiles}
      remove={remove}
    />
  );
}

function UpdateWebhook({ name }: { name: string }) {
  const [path, setPath] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    api
      .stackUpdateWebhook(name)
      .then((r) => setPath(r.path))
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
  }, [name]);

  const regenerate = async () => {
    if (!confirm('Regenerate the webhook URL? The current URL stops working immediately.')) return;
    try {
      setPath((await api.regenerateStackUpdateWebhook(name)).path);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  if (error) return <p className="muted">{error}</p>;
  if (!path) return null;
  const url = `${window.location.origin}${path}`;
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(url);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      alert('failed to copy to clipboard');
    }
  };
  return (
    <div className="section">
      <div className="section-head">
        <h2>
          <Icon name="key" /> Update webhook
        </h2>
      </div>
      <p className="page-sub">
        POST to this URL from a CI job to pull and restart every container of this stack. Anyone holding the URL can
        trigger an update, so keep it secret; regenerate it if it leaks.
      </p>
      <div className="flex-row">
        <input type="text" readOnly className="mono" value={url} style={{ flex: 1 }} onFocus={(e) => e.target.select()} />
        <button onClick={copy}>{copied ? 'Copied' : 'Copy'}</button>
        <button onClick={regenerate}>Regenerate</button>
      </div>
      <pre className="mono">curl -fsS -X POST '{url}'</pre>
    </div>
  );
}

function StackDetailBody({
  name,
  stack,
  instances,
  isAdmin,
  known,
  openEdit,
  openConfigFiles,
  remove,
}: {
  name: string;
  stack: StackState;
  instances: Assignment[];
  isAdmin: boolean;
  known: KnownNames;
  openEdit: () => void;
  openConfigFiles: () => void;
  remove: () => void;
}) {
  const [live, setLive] = useState<Record<string, InstanceState>>({});
  const [copied, setCopied] = useState(false);

  const copySpec = async () => {
    try {
      await navigator.clipboard.writeText(stack.spec);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      alert('failed to copy to clipboard');
    }
  };

  useEffect(() => {
    let cancelled = false;
    const load = () =>
      api.listInstances().then((list) => {
        if (cancelled) return;
        const byId: Record<string, InstanceState> = {};
        for (const i of list) {
          if (i.id.startsWith(`${name}.`)) byId[i.id] = i;
        }
        setLive(byId);
      });
    load();
    const timer = setInterval(load, 5000);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [name]);

  const hasIssue = (a: Assignment) => {
    const i = live[a.instance];
    if (!i) return false;
    if (a.desired === 'running' && i.state !== 'running') return true;
    if (i.health === 'unhealthy') return true;
    return false;
  };

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="stacks" />
          <div>
            <h1>{stack.name}</h1>
            <p className="page-sub">revision {stack.revision}</p>
          </div>
        </div>
        <div className="btn-row">
          <button onClick={copySpec}>
            <Icon name={copied ? 'check' : 'file'} /> {copied ? 'Copied' : 'Copy spec'}
          </button>
          {isAdmin && (
            <>
              <button onClick={openEdit}>
                <Icon name="edit" /> Redeploy
              </button>
              {(stack.configs.length > 0 || stack.secrets.length > 0) && (
                <button onClick={openConfigFiles}>
                  <Icon name="file" /> Config files
                </button>
              )}
              <button className="danger" onClick={remove}>
                <Icon name="trash" /> Remove
              </button>
            </>
          )}
        </div>
      </div>

      <div className="card-grid">
        <div className="card stat">
          <span className="stat-head">
            <Icon name="file" />
            <span className="stat-label">Revision</span>
          </span>
          <span className="stat-value">{stack.revision}</span>
        </div>
        <div className="card stat">
          <span className="stat-head">
            <Icon name="file" />
            <span className="stat-label">Configs</span>
          </span>
          <span className="stat-value">{stack.used_configs.length}</span>
        </div>
        <div className="card stat">
          <span className="stat-head">
            <Icon name="key" />
            <span className="stat-label">Secrets</span>
          </span>
          <span className="stat-value">{stack.used_secrets.length}</span>
        </div>
      </div>

      {isAdmin && <UpdateWebhook name={name} />}

      <div className="section">
        <div className="section-head">
          <h2>
            <Icon name="instances" /> Instances
          </h2>
        </div>
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Instance</th>
                <th>Node</th>
                <th>Desired</th>
                <th>State</th>
                <th>Health</th>
                <th>Migration</th>
              </tr>
            </thead>
            <tbody>
              {instances.map((a) => {
                const i = live[a.instance];
                const issue = hasIssue(a);
                return (
                  <Fragment key={a.instance}>
                    <tr>
                      <td>
                        <Link className="row-link" to={`/instances/${encodeURIComponent(a.instance)}`}>
                          {a.instance}
                        </Link>
                      </td>
                      <td>{a.node}</td>
                      <td>
                        <Badge tone={a.desired === 'running' ? 'good' : 'neutral'}>{a.desired}</Badge>
                      </td>
                      <td>
                        {i ? (
                          <InstanceStateCell instance={a.instance} state={i.state} placementError={i.placement_error} />
                        ) : (
                          '-'
                        )}
                      </td>
                      <td>
                        {i ? (
                          <Badge tone={i.health === 'unhealthy' ? 'bad' : i.health === 'healthy' ? 'good' : 'neutral'}>
                            {i.health}
                          </Badge>
                        ) : (
                          '-'
                        )}
                      </td>
                      <td>{a.migration ? `${a.migration.phase} -> ${a.migration.to}` : '-'}</td>
                    </tr>
                    {issue && (
                      <tr>
                        <td colSpan={6} style={{ paddingTop: 0 }}>
                          <div className="banner banner-bad" role="alert" style={{ marginBottom: 8 }}>
                            <Icon name="alert-circle" />
                            {a.instance} is desired {a.desired} but is {i?.state}
                            {i?.health === 'unhealthy' ? ' and unhealthy' : ''} on {a.node}. Recent logs:
                          </div>
                          <InlineInstanceLogs id={a.instance} />
                        </td>
                      </tr>
                    )}
                  </Fragment>
                );
              })}
              {Object.values(live)
                .filter((i) => i.state === 'pending_placement')
                .map((i) => (
                  <tr key={i.id}>
                    <td>{i.id}</td>
                    <td>-</td>
                    <td>
                      <Badge tone="good">running</Badge>
                    </td>
                    <td>
                      <InstanceStateCell instance={i.id} state={i.state} placementError={i.placement_error} />
                    </td>
                    <td>-</td>
                    <td>-</td>
                  </tr>
                ))}
              {instances.length === 0 && Object.keys(live).length === 0 && (
                <tr>
                  <td colSpan={6} className="empty-cell">
                    no instances
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>

      <PlacementSection stackName={stack.name} spec={stack.spec} />

      <div className="section">
        <div className="section-head">
          <h2>
            <Icon name="file" /> Spec
          </h2>
        </div>
        <StackSpecEditor value={stack.spec} editable={false} known={known} minHeight="200px" />
      </div>
    </div>
  );
}

/** A compact, non-interactive tail of an instance's log, shown inline next to
 *  a failing instance so the reason it won't start is visible without
 *  navigating away from the stack. */
function InlineInstanceLogs({ id }: { id: string }) {
  const url = `/api/instances/${encodeURIComponent(id)}/logs?follow=false&tail=30`;
  const { items, connected } = useEventSource<LogLine>(url, {
    limit: 30,
    events: ['stdout', 'stderr'],
    mapItem: (data, eventType) => {
      const line = data as { text: string; timestamp: string };
      return { timestamp: line.timestamp, message: line.text, stream: eventType as 'stdout' | 'stderr' };
    },
    dedupeKey: (line) => `${line.timestamp}|${line.stream}|${line.message}`,
  });

  return (
    <div className="log-view" aria-live="polite" style={{ maxHeight: 200 }}>
      {items.map((line, i) => (
        <div key={i}>
          <span className="faint">{line.timestamp} </span>
          <span className={line.stream === 'stderr' ? 'diff-remove' : undefined}>
            <LogMessage text={line.message} />
          </span>
        </div>
      ))}
      {items.length === 0 && (
        <div className="faint">{connected ? 'no log output yet' : 'connecting...'}</div>
      )}
      <div style={{ marginTop: 4 }}>
        <Link className="row-link" to={`/instances/${encodeURIComponent(id)}`}>
          <Icon name="terminal" /> view full logs
        </Link>
      </div>
    </div>
  );
}

/** Why each node can or cannot run this stack, and a way to fix a missing
 *  config or secret without leaving the page. One tab per service/container,
 *  since a combined table can't say which constraint belongs to which one. */
function PlacementSection({ stackName, spec }: { stackName: string; spec: string }) {
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const [requirements, setRequirements] = useState<StackRequirements | null>(null);
  const [perService, setPerService] = useState<Record<string, StackRequirements>>({});
  const [service, setService] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creatingConfig, setCreatingConfig] = useState<string | null>(null);
  const [creatingSecret, setCreatingSecret] = useState<string | null>(null);
  // A served drive has one authoritative copy any node can reach over the
  // internal network, so it does not narrow eligible nodes to whichever one
  // declared it locally the way a replicated drive does (see
  // cl-sched::place::rejection). The table needs to know which drives are
  // served to show the same "can schedule here" answer the real scheduler
  // would give.
  const [servedDrives, setServedDrives] = useState<Set<string>>(new Set());
  // A nat network takes no per-node config at all, so every node implements
  // it automatically - the same "every node is eligible, not just the one
  // that declared it" bypass servedDrives gives configs, this time for
  // `cl-sched::place::rejection`'s `everywhere_networks`.
  const [natNetworks, setNatNetworks] = useState<Set<string>>(new Set());
  // Drive name -> nodes holding its real (non-mountpoint) directory: only
  // consulted for a `drive/.` whole-drive mount, which needs the stricter
  // "pinned to the sole real holder" bar `validate_whole_drive_mounts`
  // enforces rather than the ordinary implements-or-served check.
  const [driveHolders, setDriveHolders] = useState<Record<string, string[]>>({});
  // Networks a raw `x-clouduler.expose` port is actually checked against
  // (`ClusterConfig::port_gated_networks`: public and wireguard only).
  const [portGatedNetworks, setPortGatedNetworks] = useState<Set<string>>(new Set());

  useEffect(() => {
    let cancelled = false;
    api
      .listDrives()
      .then((drives) => {
        if (cancelled) return;
        setServedDrives(new Set(drives.filter((d) => d.mode === 'served').map((d) => d.name)));
        setDriveHolders(
          Object.fromEntries(drives.map((d) => [d.name, d.mode === 'served' ? d.served_by : d.nodes.map((n) => n.node)])),
        );
      })
      .catch(() => {
        /* the table just falls back to strict (replicated-style) eligibility */
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    api
      .getConfiguration()
      .then((config) => {
        if (cancelled) return;
        const nat = Object.entries(config.networks)
          .filter(([, net]) => net.type === 'nat')
          .map(([name]) => name);
        setNatNetworks(new Set(nat));
        const gated = Object.entries(config.networks)
          .filter(([, net]) => net.type === 'public' || net.type === 'wireguard')
          .map(([name]) => name);
        setPortGatedNetworks(new Set(gated));
      })
      .catch(() => {
        /* the table just falls back to strict (per-node) eligibility */
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    api
      .validateStack(stackName, spec)
      .then((result) => {
        if (cancelled) return;
        setRequirements(result.requirements);
        setPerService(result.per_service);
        const names = Object.keys(result.per_service);
        setService((prev) => (prev && names.includes(prev) ? prev : (names[0] ?? null)));
      })
      .catch((e) => {
        if (!cancelled) setError(e instanceof ApiError ? e.message : 'failed to compute placement');
      });
    return () => {
      cancelled = true;
    };
  }, [stackName, spec]);

  if (error) {
    return (
      <p className="field-error">
        <Icon name="alert-circle" /> {error}
      </p>
    );
  }

  if (!cluster || !requirements) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading placement...
      </p>
    );
  }

  const serviceNames = Object.keys(perService);

  return (
    <div className="section">
      <div className="section-head">
        <h2>
          <Icon name="nodes" /> Placement
        </h2>
      </div>
      {serviceNames.length === 0 ? (
        <p className="muted">no services in this stack</p>
      ) : (
        <>
          <div className="segmented" role="tablist" style={{ marginBottom: 12 }}>
            {serviceNames.map((name) => (
              <button key={name} role="tab" aria-selected={service === name} onClick={() => setService(name)}>
                {name}
              </button>
            ))}
          </div>
          {service && (
            <ConstraintTable
              requirements={perService[service]}
              cluster={cluster}
              servedDrives={servedDrives}
              natNetworks={natNetworks}
              driveHolders={driveHolders}
              portGatedNetworks={portGatedNetworks}
              onCreateConfig={(n) => setCreatingConfig(n)}
              onCreateSecret={(n) => setCreatingSecret(n)}
            />
          )}
        </>
      )}
      {isAdmin && creatingConfig && <ConfigQuickCreate name={creatingConfig} onDone={() => setCreatingConfig(null)} />}
      {isAdmin && creatingSecret && (
        <div className="card section">
          <SecretForm initialName={creatingSecret} lockName onDone={() => setCreatingSecret(null)} />
        </div>
      )}
      {requirements.hosts.length > 0 && <DnsCertTable hosts={requirements.hosts} cluster={cluster} />}
    </div>
  );
}

/** DNS/certificate status for this stack's proxied hostnames. A host counts
 *  as "live"/"issued" once it has a matching entry in
 *  `dns_records`/`certificates`; otherwise `dns_status`/`cert_status` carries
 *  whatever the reconcile loop last recorded (pending, or the last error) —
 *  see `ReconcileStatus` in cl-raft's state.rs. Neither map carries a stack
 *  field, so hosts are matched by fqdn against this stack's own spec. */
function DnsCertTable({ hosts, cluster }: { hosts: string[]; cluster: NonNullable<ReturnType<typeof useClusterState>> }) {
  const liveDnsHosts = new Set(Object.values(cluster.dns_records).map((r) => r.fqdn));

  return (
    <div className="section">
      <div className="section-head">
        <h2>
          <Icon name="network" /> DNS &amp; certificates
        </h2>
      </div>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Host</th>
              <th>DNS</th>
              <th>Certificate</th>
            </tr>
          </thead>
          <tbody>
            {hosts.map((host) => (
              <tr key={host}>
                <td>{host}</td>
                <td>{liveDnsHosts.has(host) ? <Badge tone="good">live</Badge> : statusBadge(cluster.dns_status[host]) ?? <Badge tone="neutral">unknown</Badge>}</td>
                <td>
                  {cluster.certificates[host] ? (
                    <Badge tone="good">issued</Badge>
                  ) : (
                    statusBadge(cluster.cert_status[host]) ?? <Badge tone="neutral">unknown</Badge>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function ConfigQuickCreate({ name, onDone }: { name: string; onDone: () => void }) {
  const [content, setContent] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.putConfig(name, encodeText(content));
      onDone();
    } catch (e) {
      console.error(`[config] failed to save "${name}":`, e);
      setError(e instanceof ApiError ? e.message : 'failed to save config');
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="card section">
      <h3 className="flex-row">
        <Icon name="file" /> Create config {name}
      </h3>
      <div className="field">
        <label htmlFor="quick-config-content">Content</label>
        <input type="file" onChange={(e) => e.target.files?.[0]?.text().then(setContent)} />
        <textarea id="quick-config-content" rows={8} value={content} onChange={(e) => setContent(e.target.value)} />
      </div>
      {error && (
        <p className="field-error">
          <Icon name="alert-circle" /> {error}
        </p>
      )}
      <div className="btn-row">
        <button className="primary" onClick={save} disabled={busy}>
          <Icon name="check" /> Save
        </button>
        <button onClick={onDone} disabled={busy}>
          <Icon name="close" /> Cancel
        </button>
      </div>
    </div>
  );
}
