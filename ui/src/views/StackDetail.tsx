import { useEffect, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { api, ApiError, decodeText, encodeText } from '../api/client';
import type { StackRequirements, ValidationError } from '../api/types';
import { useClusterState } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { Badge } from '../components/Badge';
import { ConstraintTable } from '../components/ConstraintTable';
import { Icon } from '../components/Icon';
import { SecretForm } from './SecretForm';

export function StackDetail() {
  const { name = '' } = useParams();
  const isNew = name === '_new';
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const navigate = useNavigate();

  const stack = !isNew ? cluster?.stacks[name] : undefined;
  const [editing, setEditing] = useState(isNew);
  const [nameInput, setNameInput] = useState(isNew ? '' : name);
  const [spec, setSpec] = useState(stack?.spec ?? '');
  const [errors, setErrors] = useState<ValidationError[]>([]);
  const [requirements, setRequirements] = useState<StackRequirements | null>(null);
  const [configDrafts, setConfigDrafts] = useState<Record<string, string>>({});
  const [secretDrafts, setSecretDrafts] = useState<Record<string, string>>({});
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const openEdit = () => {
    setNameInput(name);
    setSpec(stack?.spec ?? '');
    setErrors([]);
    setRequirements(null);
    setConfigDrafts({});
    setSecretDrafts({});
    setEditing(true);
  };

  const onFile = async (file: File) => {
    setSpec(await file.text());
  };

  // Seed a draft field for every file-backed config/secret the spec
  // references, without clobbering anything already typed in.
  useEffect(() => {
    if (!requirements) return;
    setConfigDrafts((prev) => {
      const next = { ...prev };
      for (const key of requirements.file_configs) {
        if (!(key in next)) {
          const existing = cluster?.configs[key];
          next[key] = existing ? decodeText(existing.content) : '';
        }
      }
      return next;
    });
    setSecretDrafts((prev) => {
      const next = { ...prev };
      for (const key of requirements.file_secrets) {
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
          <textarea
            id="stack-spec"
            rows={20}
            value={spec}
            onChange={(e) => setSpec(e.target.value)}
            placeholder="services:\n  web:\n    image: nginx:latest"
          />
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

        {requirements && (requirements.file_configs.length > 0 || requirements.file_secrets.length > 0) && (
          <div className="section">
            <div className="section-head">
              <h2>
                <Icon name="file" /> Config &amp; secret files
              </h2>
            </div>
            <p className="page-sub">
              These come from this spec's <code>configs:</code>/<code>secrets:</code> blocks. Leave a field blank to
              keep its current value.
            </p>
            {requirements.file_configs.map((key) => (
              <div className="field" key={`config-${key}`}>
                <label htmlFor={`config-${key}`}>
                  config: {key} {!cluster?.configs[key] && <Badge tone="warn">new</Badge>}
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
            {requirements.file_secrets.map((key) => (
              <div className="field" key={`secret-${key}`}>
                <label htmlFor={`secret-${key}`}>
                  secret: {key} {!cluster?.secrets[key] && <Badge tone="warn">new</Badge>}
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
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="stacks" />
          <div>
            <h1>{stack.name}</h1>
            <p className="page-sub">revision {stack.revision}</p>
          </div>
        </div>
        {isAdmin && (
          <div className="btn-row">
            <button onClick={openEdit}>
              <Icon name="edit" /> Redeploy
            </button>
            <button className="danger" onClick={remove}>
              <Icon name="trash" /> Remove
            </button>
          </div>
        )}
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
          <span className="stat-value">{stack.configs.length}</span>
        </div>
        <div className="card stat">
          <span className="stat-head">
            <Icon name="key" />
            <span className="stat-label">Secrets</span>
          </span>
          <span className="stat-value">{stack.secrets.length}</span>
        </div>
      </div>

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
                <th>Migration</th>
              </tr>
            </thead>
            <tbody>
              {instances.map((a) => (
                <tr key={a.instance}>
                  <td>{a.instance}</td>
                  <td>{a.node}</td>
                  <td>
                    <Badge tone={a.desired === 'running' ? 'good' : 'neutral'}>{a.desired}</Badge>
                  </td>
                  <td>{a.migration ? `${a.migration.phase} -> ${a.migration.to}` : '-'}</td>
                </tr>
              ))}
              {instances.length === 0 && (
                <tr>
                  <td colSpan={4} className="empty-cell">
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
        <pre>{stack.spec}</pre>
      </div>
    </div>
  );
}

/** Why each node can or cannot run this stack, and a way to fix a missing
 *  config or secret without leaving the page. */
function PlacementSection({ stackName, spec }: { stackName: string; spec: string }) {
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const [requirements, setRequirements] = useState<StackRequirements | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creatingConfig, setCreatingConfig] = useState<string | null>(null);
  const [creatingSecret, setCreatingSecret] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    api
      .validateStack(stackName, spec)
      .then((result) => {
        if (!cancelled) setRequirements(result.requirements);
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

  return (
    <div className="section">
      <div className="section-head">
        <h2>
          <Icon name="nodes" /> Placement
        </h2>
      </div>
      <ConstraintTable
        requirements={requirements}
        cluster={cluster}
        onCreateConfig={(n) => setCreatingConfig(n)}
        onCreateSecret={(n) => setCreatingSecret(n)}
      />
      {isAdmin && creatingConfig && <ConfigQuickCreate name={creatingConfig} onDone={() => setCreatingConfig(null)} />}
      {isAdmin && creatingSecret && (
        <div className="card section">
          <SecretForm initialName={creatingSecret} lockName onDone={() => setCreatingSecret(null)} />
        </div>
      )}
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
