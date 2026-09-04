// Where a drive's restic backup history is kept. A destination only stores
// where and how to reach a repository plus the name of the sealed secret
// holding its password/credentials - never the secret value itself, see
// BackupDestinationSummary in api/types.ts.

import { useEffect, useState } from 'react';
import { api, ApiError } from '../../api/client';
import type { BackupDestinationKind, BackupDestinationSummary } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Icon } from '../../components/Icon';
import { NumberField, TextField } from '../../components/FormFields';

type Kind = BackupDestinationKind['kind'];

const KIND_LABEL: Record<Kind, string> = {
  nodelocal: 'Cluster node',
  ssh: 'SSH server',
  s3: 'S3 bucket',
};

interface Draft {
  id: string;
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

const emptyDraft = (node: string): Draft => ({
  id: '',
  name: '',
  kind: 'nodelocal',
  node,
  path: '/srv/backups',
  host: '',
  port: 22,
  user: '',
  bucket: '',
  endpoint: '',
  region: '',
  prefix: '',
  password: '',
  ssh_private_key: '',
  s3_access_key_id: '',
  s3_secret_access_key: '',
});

function useNodeNames(): string[] {
  const [names, setNames] = useState<string[]>([]);
  useEffect(() => {
    api
      .getConfiguration()
      .then((cfg) => setNames(Object.keys(cfg.nodes)))
      .catch(() => setNames([]));
  }, []);
  return names;
}

function kindOf(d: BackupDestinationSummary): BackupDestinationKind {
  return d.kind;
}

function summarizeKind(kind: BackupDestinationKind): string {
  switch (kind.kind) {
    case 'nodelocal':
      return `${kind.node}:${kind.path}`;
    case 'ssh':
      return `${kind.user}@${kind.host}:${kind.port}${kind.path}`;
    case 's3':
      return `${kind.bucket}${kind.prefix ? `/${kind.prefix}` : ''} @ ${kind.endpoint}`;
  }
}

export function Destinations() {
  const { isAdmin } = useAuth();
  const nodeNames = useNodeNames();
  const [destinations, setDestinations] = useState<BackupDestinationSummary[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showForm, setShowForm] = useState(false);
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState<Draft>(emptyDraft(''));

  const load = () => {
    api
      .listBackupDestinations()
      .then(setDestinations)
      .catch((e) => setError(e instanceof ApiError ? e.message : 'failed to load backup destinations'));
  };

  useEffect(load, []);

  const startCreate = () => {
    setEditing(null);
    setDraft(emptyDraft(nodeNames[0] ?? ''));
    setShowForm(true);
  };

  const startEdit = (d: BackupDestinationSummary) => {
    const base = emptyDraft(nodeNames[0] ?? '');
    setEditing(d.id);
    if (d.kind.kind === 'nodelocal') {
      setDraft({ ...base, id: d.id, name: d.name, kind: 'nodelocal', node: d.kind.node, path: d.kind.path });
    } else if (d.kind.kind === 'ssh') {
      setDraft({
        ...base,
        id: d.id,
        name: d.name,
        kind: 'ssh',
        host: d.kind.host,
        port: d.kind.port,
        user: d.kind.user,
        path: d.kind.path,
      });
    } else {
      setDraft({
        ...base,
        id: d.id,
        name: d.name,
        kind: 's3',
        bucket: d.kind.bucket,
        endpoint: d.kind.endpoint,
        region: d.kind.region ?? '',
        prefix: d.kind.prefix ?? '',
      });
    }
    setShowForm(true);
  };

  const save = async () => {
    setError(null);
    const id = editing ?? (draft.name.trim().toLowerCase().replace(/[^a-z0-9-]+/g, '-') || `dest-${Date.now()}`);
    let kind: BackupDestinationKind;
    if (draft.kind === 'nodelocal') {
      kind = { kind: 'nodelocal', node: draft.node, path: draft.path };
    } else if (draft.kind === 'ssh') {
      kind = { kind: 'ssh', host: draft.host, port: draft.port, user: draft.user, path: draft.path };
    } else {
      kind = {
        kind: 's3',
        bucket: draft.bucket,
        endpoint: draft.endpoint,
        region: draft.region || null,
        prefix: draft.prefix || null,
      };
    }
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
      setShowForm(false);
      setEditing(null);
      load();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'failed to save backup destination');
    }
  };

  const remove = async (id: string) => {
    if (!confirm('Remove this backup destination? Schedules still referencing it will stop running.')) return;
    try {
      await api.removeBackupDestination(id);
      load();
    } catch (e) {
      alert(e instanceof ApiError ? e.message : 'failed to remove backup destination');
    }
  };

  if (!destinations) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading backup destinations...
      </p>
    );
  }

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="cloud" />
          <div>
            <h1>Backup destinations</h1>
            <p className="page-sub">Places restic keeps a drive's backup history: a cluster node, an SSH server, or an S3 bucket.</p>
          </div>
        </div>
      </div>

      <div className="section">
        <div className="section-head">
          <h2>
            {destinations.length} destination{destinations.length === 1 ? '' : 's'}
          </h2>
          {isAdmin && (
            <button onClick={showForm ? () => setShowForm(false) : startCreate}>
              <Icon name={showForm ? 'close' : 'plus'} />
              {showForm ? 'Cancel' : 'New destination'}
            </button>
          )}
        </div>
        {error && (
          <div className="banner banner-warn">
            <Icon name="alert" /> {error}
          </div>
        )}

        {showForm && (
          <div className="card section">
            <TextField
              id="dest-name"
              label="Name"
              value={draft.name}
              onChange={(name) => setDraft({ ...draft, name })}
              hint="Shown in schedules and history; the id is derived from it."
            />

            <div className="field">
              <label>Kind</label>
              <div className="btn-row" role="group" aria-label="Destination kind">
                {(Object.keys(KIND_LABEL) as Kind[]).map((k) => (
                  <button
                    key={k}
                    type="button"
                    className={draft.kind === k ? 'primary' : ''}
                    aria-pressed={draft.kind === k}
                    onClick={() => setDraft({ ...draft, kind: k })}
                  >
                    {KIND_LABEL[k]}
                  </button>
                ))}
              </div>
            </div>

            {draft.kind === 'nodelocal' && (
              <>
                <div className="field">
                  <label htmlFor="dest-node">Node</label>
                  <select id="dest-node" value={draft.node} onChange={(e) => setDraft({ ...draft, node: e.target.value })}>
                    {nodeNames.map((n) => (
                      <option key={n} value={n}>
                        {n}
                      </option>
                    ))}
                  </select>
                  <span className="field-hint">
                    If this node doesn't already hold the drive, it's synced there once before each backup - not kept
                    as a standing replica.
                  </span>
                </div>
                <TextField
                  id="dest-path"
                  label="Path"
                  value={draft.path}
                  onChange={(path) => setDraft({ ...draft, path })}
                  hint="Directory on that node to hold the restic repository."
                />
              </>
            )}

            {draft.kind === 'ssh' && (
              <>
                <TextField id="dest-host" label="Host" value={draft.host} onChange={(host) => setDraft({ ...draft, host })} />
                <NumberField id="dest-port" label="Port" value={draft.port} onChange={(port) => setDraft({ ...draft, port })} />
                <TextField id="dest-user" label="User" value={draft.user} onChange={(user) => setDraft({ ...draft, user })} />
                <TextField
                  id="dest-path-ssh"
                  label="Path"
                  value={draft.path}
                  onChange={(path) => setDraft({ ...draft, path })}
                  hint="Directory on the remote host to hold the restic repository."
                />
                <TextField
                  id="dest-ssh-key"
                  label="Private key"
                  value={draft.ssh_private_key}
                  onChange={(ssh_private_key) => setDraft({ ...draft, ssh_private_key })}
                  hint={editing ? 'Leave blank to keep the current key.' : 'Pasted, sealed, and stored - never shown again.'}
                />
              </>
            )}

            {draft.kind === 's3' && (
              <>
                <TextField id="dest-bucket" label="Bucket" value={draft.bucket} onChange={(bucket) => setDraft({ ...draft, bucket })} />
                <TextField
                  id="dest-endpoint"
                  label="Endpoint"
                  value={draft.endpoint}
                  onChange={(endpoint) => setDraft({ ...draft, endpoint })}
                  hint="e.g. s3.amazonaws.com or a compatible provider's endpoint."
                />
                <TextField id="dest-region" label="Region" value={draft.region} onChange={(region) => setDraft({ ...draft, region })} />
                <TextField id="dest-prefix" label="Prefix" value={draft.prefix} onChange={(prefix) => setDraft({ ...draft, prefix })} />
                <TextField
                  id="dest-s3-key"
                  label="Access key ID"
                  value={draft.s3_access_key_id}
                  onChange={(s3_access_key_id) => setDraft({ ...draft, s3_access_key_id })}
                  hint={editing ? 'Leave blank to keep the current credentials.' : undefined}
                />
                <TextField
                  id="dest-s3-secret"
                  label="Secret access key"
                  value={draft.s3_secret_access_key}
                  onChange={(s3_secret_access_key) => setDraft({ ...draft, s3_secret_access_key })}
                />
              </>
            )}

            <TextField
              id="dest-password"
              label="Repository password"
              value={draft.password}
              onChange={(password) => setDraft({ ...draft, password })}
              hint={editing ? 'Leave blank to keep the current password.' : 'Leave blank to generate one automatically.'}
            />

            <button onClick={save}>
              <Icon name="check" /> {editing ? 'Save' : 'Create'}
            </button>
          </div>
        )}

        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Kind</th>
                <th>Location</th>
                <th>Credentials</th>
                {isAdmin && <th>Actions</th>}
              </tr>
            </thead>
            <tbody>
              {destinations.map((d) => (
                <tr key={d.id}>
                  <td>{d.name}</td>
                  <td>{KIND_LABEL[kindOf(d).kind]}</td>
                  <td className="mono">{summarizeKind(kindOf(d))}</td>
                  <td>{d.has_credentials ? 'set' : d.kind.kind === 'nodelocal' ? '-' : 'missing'}</td>
                  {isAdmin && (
                    <td>
                      <button onClick={() => startEdit(d)}>
                        <Icon name="edit" /> Edit
                      </button>
                      <button className="danger" onClick={() => remove(d.id)}>
                        <Icon name="trash" /> Remove
                      </button>
                    </td>
                  )}
                </tr>
              ))}
              {destinations.length === 0 && (
                <tr>
                  <td colSpan={5} className="empty-cell">
                    no backup destinations
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
