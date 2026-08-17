import { useEffect, useState } from 'react';
import { api, ApiError } from '../api/client';
import type { ClusterKeyStatus } from '../api/types';
import { useClusterState } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { SecretForm } from './SecretForm';
import { Badge } from '../components/Badge';

export function ConfigsSecrets() {
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const [showSecretForm, setShowSecretForm] = useState(false);
  const [showConfigForm, setShowConfigForm] = useState(false);
  const [configName, setConfigName] = useState('');
  const [configContent, setConfigContent] = useState('');
  const [error, setError] = useState<string | null>(null);

  if (!cluster) return <p className="muted">Loading...</p>;

  const configs = Object.values(cluster.configs);
  const secrets = Object.values(cluster.secrets);

  const saveConfig = async () => {
    setError(null);
    try {
      await api.putConfig(configName.trim(), btoa(configContent));
      setShowConfigForm(false);
      setConfigName('');
      setConfigContent('');
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'failed to save config');
    }
  };

  const removeConfig = async (name: string) => {
    if (!confirm(`Remove config ${name}?`)) return;
    try {
      await api.removeConfig(name);
    } catch (e) {
      alert(e instanceof ApiError ? e.message : 'failed to remove');
    }
  };

  const removeSecret = async (name: string) => {
    if (!confirm(`Remove secret ${name}?`)) return;
    try {
      await api.removeSecret(name);
    } catch (e) {
      alert(e instanceof ApiError ? e.message : 'failed to remove');
    }
  };

  const stacksUsing = (kind: 'configs' | 'secrets', name: string) =>
    Object.values(cluster.stacks)
      .filter((s) => s[kind].includes(name))
      .map((s) => s.name);

  return (
    <div>
      <div className="page-header">
        <h1>Configs and secrets</h1>
      </div>

      <ClusterKeyPanel />

      <div className="section">
        <div className="page-header">
          <h2>Configs</h2>
          {isAdmin && <button onClick={() => setShowConfigForm((v) => !v)}>{showConfigForm ? 'Cancel' : 'New config'}</button>}
        </div>
        {showConfigForm && (
          <div className="card section">
            <div className="field">
              <label htmlFor="config-name">Name</label>
              <input id="config-name" type="text" value={configName} onChange={(e) => setConfigName(e.target.value)} />
            </div>
            <div className="field">
              <label htmlFor="config-content">Content</label>
              <textarea id="config-content" rows={8} value={configContent} onChange={(e) => setConfigContent(e.target.value)} />
            </div>
            {error && <p className="field-error">{error}</p>}
            <button className="primary" onClick={saveConfig}>
              Save
            </button>
          </div>
        )}
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Revision</th>
                <th>Owner</th>
                <th>Used by</th>
                {isAdmin && <th>Actions</th>}
              </tr>
            </thead>
            <tbody>
              {configs.map((c) => (
                <tr key={c.name}>
                  <td>{c.name}</td>
                  <td>{c.revision}</td>
                  <td>{c.owner ?? '-'}</td>
                  <td>{stacksUsing('configs', c.name).join(', ') || '-'}</td>
                  {isAdmin && (
                    <td>
                      <button onClick={() => removeConfig(c.name)}>Remove</button>
                    </td>
                  )}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>

      <div className="section">
        <div className="page-header">
          <h2>Secrets</h2>
          {isAdmin && (
            <button onClick={() => setShowSecretForm((v) => !v)}>{showSecretForm ? 'Cancel' : 'New secret'}</button>
          )}
        </div>
        {showSecretForm && <SecretForm onDone={() => setShowSecretForm(false)} />}
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Revision</th>
                <th>Owner</th>
                <th>Key fingerprint</th>
                <th>Used by</th>
                {isAdmin && <th>Actions</th>}
              </tr>
            </thead>
            <tbody>
              {secrets.map((s) => (
                <tr key={s.name}>
                  <td>{s.name}</td>
                  <td>{s.revision}</td>
                  <td>{s.owner ?? '-'}</td>
                  <td className="mono">{s.key_fingerprint.slice(0, 12)}</td>
                  <td>{stacksUsing('secrets', s.name).join(', ') || '-'}</td>
                  {isAdmin && (
                    <td>
                      <button onClick={() => removeSecret(s.name)}>Remove</button>
                    </td>
                  )}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}

function ClusterKeyPanel() {
  const { isAdmin } = useAuth();
  const [status, setStatus] = useState<ClusterKeyStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [uploading, setUploading] = useState(false);

  useEffect(() => {
    api.clusterKey().then(setStatus).catch((e) => setError(e instanceof ApiError ? e.message : String(e)));
  }, []);

  const upload = async (file: File) => {
    setError(null);
    try {
      const buf = await file.arrayBuffer();
      const b64 = btoa(String.fromCharCode(...new Uint8Array(buf)));
      await api.uploadClusterKey(b64);
      setStatus(await api.clusterKey());
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'upload failed');
    } finally {
      setUploading(false);
    }
  };

  return (
    <div className="card section">
      <h2>Cluster key</h2>
      {status ? (
        <>
          <p>
            Fingerprint: <span className="mono">{status.fingerprint ?? 'none'}</span>
          </p>
          <p>
            This node holds the key: <Badge tone={status.held ? 'good' : 'bad'}>{status.held ? 'yes' : 'no'}</Badge>
          </p>
          {!status.held && (
            <p className="banner banner-warn">
              Without the key this node cannot decrypt secrets or run containers that need one.
            </p>
          )}
        </>
      ) : (
        <p className="muted">Loading...</p>
      )}
      {isAdmin && !status?.held && (
        <div className="field">
          <label htmlFor="key-upload">Upload key file (after rebuilding a cluster following an outage)</label>
          <input
            id="key-upload"
            type="file"
            disabled={uploading}
            onChange={(e) => e.target.files?.[0] && upload(e.target.files[0])}
          />
        </div>
      )}
      {error && <p className="field-error">{error}</p>}
    </div>
  );
}
