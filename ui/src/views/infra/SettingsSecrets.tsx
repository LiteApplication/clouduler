// Settings > Secrets & configs: the cluster key, then the secrets and configs
// lists. Create secret reuses SecretForm (paste, file or generate).

import { useEffect, useState } from 'react';
import { api, ApiError, encodeText } from '../../api/client';
import type { ClusterKeyStatus } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Banner, BottomSheet, Button, Panel, Tag, TextField, useConfirm, useToast } from '../../ds';
import { useClusterState } from '../../state/store';
import { SecretForm } from '../SecretForm';
import { DataTable } from './table';

export function SettingsSecrets({ sheetOpen, onOpen, onClose }: { sheetOpen: boolean; onOpen: () => void; onClose: () => void }) {
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const confirm = useConfirm();
  const toast = useToast();
  const [configSheet, setConfigSheet] = useState(false);
  const [configName, setConfigName] = useState('');
  const [configContent, setConfigContent] = useState('');
  const [error, setError] = useState<string | null>(null);

  if (!cluster) return <p className="cl-muted">Loading…</p>;
  const configs = Object.values(cluster.configs);
  const secrets = Object.values(cluster.secrets);
  const usedBy = (kind: 'used_configs' | 'used_secrets', name: string) =>
    Object.values(cluster.stacks)
      .filter((s) => s[kind].includes(name))
      .map((s) => s.name)
      .join(', ') || '-';

  const saveConfig = async () => {
    setError(null);
    try {
      await api.putConfig(configName.trim(), encodeText(configContent));
      setConfigSheet(false);
      setConfigName('');
      setConfigContent('');
      toast(`Saved ${configName.trim()}.`);
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Could not save that config.');
    }
  };

  const remove = async (kind: 'config' | 'secret', name: string) => {
    const ok = await confirm({
      title: `Remove ${kind} ${name}?`,
      body: kind === 'secret' ? 'Stacks that reference it will fail to start until it is created again.' : undefined,
      confirmLabel: `Remove ${name}`,
      confirmVariant: 'danger',
    });
    if (!ok) return;
    try {
      await (kind === 'config' ? api.removeConfig(name) : api.removeSecret(name));
      toast(`Removed ${name}.`);
    } catch (e) {
      toast(e instanceof ApiError ? e.message : 'Could not remove that.', { tone: 'danger' });
    }
  };

  return (
    <div className="stack">
      <ClusterKeyPanel />
      <Panel
        title="Secrets"
        flush
        actions={isAdmin && <Button size="sm" icon="plus" onClick={onOpen}>Create secret</Button>}
      >
        <p className="cl-sm cl-muted" style={{ padding: '0 var(--space-4)' }}>
          Clouduler stores these sealed and never shows them again.
        </p>
        <DataTable
          caption="Secrets"
          rows={secrets.map((s) => ({ ...s, id: s.name }))}
          empty="No secrets yet."
          columns={[
            { key: 'name', header: 'Name', render: (s) => <code>{s.name}</code> },
            { key: 'rev', header: 'Revision', render: (s) => s.revision },
            { key: 'owner', header: 'Owner', render: (s) => s.owner ?? '-' },
            { key: 'fp', header: 'Key', render: (s) => <code>{s.key_fingerprint.slice(0, 12)}</code> },
            { key: 'used', header: 'Used by', render: (s) => usedBy('used_secrets', s.name) },
            ...(isAdmin
              ? [{ key: 'act', header: '', render: (s: (typeof secrets)[number]) => (
                  <Button iconOnly size="sm" variant="ghost" icon="trash" label={`Remove ${s.name}`} onClick={() => void remove('secret', s.name)} />
                ) }]
              : []),
          ]}
        />
      </Panel>
      <Panel title="Configs" flush actions={isAdmin && <Button size="sm" icon="plus" onClick={() => setConfigSheet(true)}>New config</Button>}>
        <DataTable
          caption="Configs"
          rows={configs.map((c) => ({ ...c, id: c.name }))}
          empty="No configs yet."
          columns={[
            { key: 'name', header: 'Name', render: (c) => <code>{c.name}</code> },
            { key: 'rev', header: 'Revision', render: (c) => c.revision },
            { key: 'owner', header: 'Owner', render: (c) => c.owner ?? '-' },
            { key: 'used', header: 'Used by', render: (c) => usedBy('used_configs', c.name) },
            ...(isAdmin
              ? [{ key: 'act', header: '', render: (c: (typeof configs)[number]) => (
                  <Button iconOnly size="sm" variant="ghost" icon="trash" label={`Remove ${c.name}`} onClick={() => void remove('config', c.name)} />
                ) }]
              : []),
          ]}
        />
      </Panel>

      <BottomSheet open={sheetOpen} title="Create a secret" onClose={onClose}>
        <SecretForm onDone={onClose} />
      </BottomSheet>
      <BottomSheet
        open={configSheet}
        title="New config"
        onClose={() => setConfigSheet(false)}
        footer={<Button variant="primary" onClick={() => void saveConfig()} disabled={!configName.trim()}>Save config</Button>}
      >
        <TextField label="Name" value={configName} onChange={(e) => setConfigName(e.target.value)} mono />
        <TextField label="Content" multiline rows={8} mono value={configContent} onChange={(e) => setConfigContent(e.target.value)} error={error ?? undefined} />
      </BottomSheet>
    </div>
  );
}

function ClusterKeyPanel() {
  const { isAdmin } = useAuth();
  const [status, setStatus] = useState<ClusterKeyStatus | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api.clusterKey().then(setStatus).catch((e) => setError(e instanceof ApiError ? e.message : String(e)));
  }, []);

  const upload = async (file: File) => {
    setError(null);
    try {
      const buf = await file.arrayBuffer();
      await api.uploadClusterKey(btoa(String.fromCharCode(...new Uint8Array(buf))));
      setStatus(await api.clusterKey());
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Upload failed.');
    }
  };

  return (
    <Panel title="Cluster key" actions={status && <Tag tone={status.held ? 'ok' : 'danger'}>{status.held ? 'held here' : 'not held here'}</Tag>}>
      <p className="cl-sm cl-muted">Seals every secret and signs node traffic. Keep a copy somewhere safe.</p>
      {status && <p>Fingerprint <code>{status.fingerprint ?? 'none'}</code></p>}
      {status && !status.held && (
        <Banner tone="warn">Without the key this node cannot decrypt secrets or run containers that need one.</Banner>
      )}
      {isAdmin && status && !status.held && (
        <div className="cl-field">
          <label className="cl-label" htmlFor="key-upload">Upload cluster key</label>
          <input id="key-upload" type="file" onChange={(e) => e.target.files?.[0] && void upload(e.target.files[0])} />
        </div>
      )}
      {error && <Banner tone="danger">{error}</Banner>}
    </Panel>
  );
}
