import { useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { api, ApiError } from '../api/client';
import type { ValidationError } from '../api/types';
import { useClusterState } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { Badge } from '../components/Badge';

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
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const openEdit = () => {
    setNameInput(name);
    setSpec(stack?.spec ?? '');
    setErrors([]);
    setEditing(true);
  };

  const onFile = async (file: File) => {
    setSpec(await file.text());
  };

  const validate = async () => {
    setSubmitError(null);
    try {
      const result = await api.validateStack(nameInput || 'preview', spec);
      setErrors(result.errors);
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
    try {
      const ok = await validate();
      if (!ok) return;
      await api.deployStack(nameInput.trim(), { spec });
      navigate(`/stacks/${encodeURIComponent(nameInput.trim())}`);
      setEditing(false);
    } catch (e) {
      setSubmitError(e instanceof ApiError ? e.message : 'deploy failed');
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
    return <p className="muted">No such stack.</p>;
  }

  if (editing) {
    return (
      <div>
        <div className="page-header">
          <h1>{isNew ? 'Deploy stack' : `Redeploy ${name}`}</h1>
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
            <ul style={{ margin: 0, paddingLeft: 18 }}>
              {errors.map((err, i) => (
                <li key={i}>
                  <span className="mono">{err.path}</span>: {err.message}
                </li>
              ))}
            </ul>
          </div>
        )}
        {submitError && <p className="field-error">{submitError}</p>}
        <div className="btn-row">
          <button onClick={validate} disabled={busy}>
            Validate
          </button>
          <button className="primary" onClick={deploy} disabled={busy}>
            {isNew ? 'Deploy' : 'Redeploy'}
          </button>
          <button onClick={() => (isNew ? navigate('/stacks') : setEditing(false))} disabled={busy}>
            Cancel
          </button>
        </div>
      </div>
    );
  }

  if (!stack) return <p className="muted">Loading...</p>;

  const instances = cluster
    ? Object.values(cluster.assignments).filter((a) => a.instance.startsWith(`${name}.`))
    : [];

  return (
    <div>
      <div className="page-header">
        <h1>{stack.name}</h1>
        {isAdmin && (
          <div className="btn-row">
            <button onClick={openEdit}>Redeploy</button>
            <button className="danger" onClick={remove}>
              Remove
            </button>
          </div>
        )}
      </div>

      <div className="card-grid">
        <div className="card stat">
          <span className="stat-label">Revision</span>
          <span className="stat-value">{stack.revision}</span>
        </div>
        <div className="card stat">
          <span className="stat-label">Configs</span>
          <span className="stat-value">{stack.configs.length}</span>
        </div>
        <div className="card stat">
          <span className="stat-label">Secrets</span>
          <span className="stat-value">{stack.secrets.length}</span>
        </div>
      </div>

      <div className="section">
        <h2>Instances</h2>
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
                  <td colSpan={4} className="muted">
                    no instances
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>

      <div className="section">
        <h2>Spec</h2>
        <pre>{stack.spec}</pre>
      </div>
    </div>
  );
}
