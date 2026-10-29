// Settings > Access: pending OAuth requests, API keys, connected AI clients
// and the signed-in identity, plus how to point a tool at this cluster's MCP.

import { useCallback, useEffect, useState } from 'react';
import { api, ApiError } from '../../api/client';
import type { ApiKey, ApiKeyCreated, McpClient, McpGrant, PendingOAuthRequest } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Banner, BottomSheet, Button, Chip, KV, Panel, Tag, TextField, useConfirm, useToast } from '../../ds';
import { relativeTime } from '../../lib/format';

const SERVER_NAME = 'clouduler';

/** Cursor's install deep link carries the server config as base64 json. */
export function cursorInstallLink(name: string, url: string): string {
  const config = btoa(JSON.stringify({ url }));
  return `cursor://anysphere.cursor-deeplink/mcp/install?name=${encodeURIComponent(name)}&config=${encodeURIComponent(config)}`;
}

/** VS Code's takes the whole server definition, url encoded, as the query. */
export function vscodeInstallLink(name: string, url: string): string {
  return `vscode:mcp/install?${encodeURIComponent(JSON.stringify({ name, type: 'http', url }))}`;
}

function CopyRow({ label, text }: { label: string; text: string }) {
  const toast = useToast();
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      toast(`Copied ${label}.`);
    } catch {
      toast('Could not copy. Select the text instead.', { tone: 'warn' });
    }
  };
  return (
    <div className="row" style={{ flexWrap: 'nowrap' }}>
      <pre className="codeblock" style={{ flex: 1 }} aria-label={label}>{text}</pre>
      <Button iconOnly variant="ghost" icon="copy" label={`Copy ${label}`} onClick={() => void copy()} />
    </div>
  );
}

export function SettingsAccess({ sheetOpen, onClose }: { sheetOpen: boolean; onClose: () => void }) {
  const { isAdmin, identity } = useAuth();
  const confirm = useConfirm();
  const toast = useToast();
  const mcpUrl = `${window.location.origin}/mcp`;
  const [clients, setClients] = useState<McpClient[] | null>(null);
  const [keys, setKeys] = useState<ApiKey[] | null>(null);
  const [requests, setRequests] = useState<PendingOAuthRequest[]>([]);
  const [grants, setGrants] = useState<Record<string, McpGrant>>({});
  const [error, setError] = useState<string | null>(null);
  const [keyName, setKeyName] = useState('');
  const [created, setCreated] = useState<ApiKeyCreated | null>(null);

  const load = useCallback(async () => {
    const msg = (e: unknown) => (e instanceof ApiError ? e.message : 'Could not load access.');
    await Promise.all([
      api.listMcpClients().then(setClients, (e) => setError(msg(e))),
      api.listKeys().then(setKeys, () => setKeys([])),
      api.listOAuthRequests().then(setRequests, () => setRequests([])),
    ]);
  }, []);
  useEffect(() => {
    void load();
  }, [load]);

  const fail = (e: unknown, fallback: string) => toast(e instanceof ApiError ? e.message : fallback, { tone: 'danger' });

  const revoke = async (c: McpClient) => {
    if (!(await confirm({ title: `Revoke ${c.client_name}?`, body: 'It loses access immediately.', confirmLabel: `Revoke ${c.client_name}`, confirmVariant: 'danger' }))) return;
    try {
      await api.revokeMcpClient(c.client_id);
      await load();
    } catch (e) {
      fail(e, 'Could not revoke that client.');
    }
  };

  const deleteKey = async (k: ApiKey) => {
    if (!(await confirm({ title: `Delete ${k.name}?`, body: 'Anything using this key stops working.', confirmLabel: `Delete ${k.name}`, confirmVariant: 'danger' }))) return;
    try {
      await api.deleteKey(k.id);
      await load();
    } catch (e) {
      fail(e, 'Could not delete that key.');
    }
  };

  const answer = async (r: PendingOAuthRequest, grant: McpGrant | 'deny') => {
    try {
      const res = grant === 'deny' ? await api.denyOAuthRequest(r.id) : await api.approveOAuthRequest(r.id, grant);
      toast(grant === 'deny' ? `Denied ${r.client_name}.` : `Approved ${r.client_name}.`);
      void res;
      await load();
    } catch (e) {
      fail(e, 'Could not answer that request.');
    }
  };

  const createKey = async () => {
    try {
      setCreated(await api.createKey(keyName.trim()));
      setKeyName('');
      await load();
    } catch (e) {
      fail(e, 'Could not create that key.');
    }
  };
  const closeCreate = () => {
    setCreated(null);
    onClose();
  };

  return (
    <div className="stack">
      {error && <Banner tone="danger">{error}</Banner>}
      {requests.map((r) => {
        const grant = grants[r.id] ?? r.requested_grant;
        return (
          <Panel key={r.id} title={<>{r.client_name} wants to connect <Tag tone="warn">waiting for you</Tag></>}>
            <KV items={[
              ['Redirects to', r.redirect_host],
              ['Asks for', r.requested_grant === 'admin' ? 'admin' : 'read-only'],
              ['Answered on', r.node],
            ]} />
            <div className="chips" role="group" aria-label="Access to grant">
              {(['read', 'admin'] as const).map((g) => (
                <Chip key={g} label={g === 'admin' ? 'Admin' : 'Read-only'} selected={grant === g} onClick={() => setGrants({ ...grants, [r.id]: g })} />
              ))}
            </div>
            {isAdmin && (
              <div className="row">
                <Button variant="danger" icon="x" onClick={() => void answer(r, 'deny')}>Deny {r.client_name}</Button>
                <Button variant="primary" icon="check" onClick={() => void answer(r, grant)}>
                  {grant === 'admin' ? 'Approve admin' : 'Approve read-only'}
                </Button>
              </div>
            )}
          </Panel>
        );
      })}

      <Panel title="API keys">
        <p className="cl-sm cl-muted">Shown once, when created.</p>
        {keys?.length === 0 && <p className="cl-sm cl-muted">No API keys yet.</p>}
        {keys?.map((k) => (
          <div className="li" key={k.id}>
            <strong>{k.name}</strong>
            <span className="cl-sm cl-muted">created {relativeTime(k.created_at)}</span>
            <span style={{ flex: 1 }} />
            {isAdmin && <Button iconOnly size="sm" variant="ghost" icon="trash" label={`Delete ${k.name}`} onClick={() => void deleteKey(k)} />}
          </div>
        ))}
      </Panel>

      <Panel title="Connected AI clients">
        {clients?.length === 0 && <p className="cl-sm cl-muted">No tool has been approved yet.</p>}
        {clients?.map((c) => (
          <div className="li" key={c.client_id}>
            <strong>{c.client_name}</strong>
            <Tag tone={c.grant === 'admin' ? 'warn' : undefined}>{c.grant === 'admin' ? 'admin' : 'read-only'}</Tag>
            <span className="cl-sm cl-muted">approved by {c.approved_by} {relativeTime(c.created_at)}</span>
            <span style={{ flex: 1 }} />
            {isAdmin && <Button iconOnly size="sm" variant="ghost" icon="trash" label={`Revoke ${c.client_name}`} onClick={() => void revoke(c)} />}
          </div>
        ))}
        <h3 className="h3">Connect a tool</h3>
        <p className="cl-sm cl-muted">Each tool signs in through your browser and asks you to approve it, choosing read-only or admin access.</p>
        <CopyRow label="MCP URL" text={mcpUrl} />
        <CopyRow label="Claude Code command" text={`claude mcp add --transport http ${SERVER_NAME} ${mcpUrl}`} />
        <div className="row">
          <a href="https://claude.ai/settings/connectors" target="_blank" rel="noreferrer">Claude.ai connectors</a>
          <a href={cursorInstallLink(SERVER_NAME, mcpUrl)}>Add to Cursor</a>
          <a href={vscodeInstallLink(SERVER_NAME, mcpUrl)}>Add to VS Code</a>
        </div>
      </Panel>

      <Panel title="You">
        <p>{identity?.username ?? 'Signed in'} · <Tag tone={isAdmin ? 'brand' : undefined}>{isAdmin ? 'admin' : 'read-only'}</Tag></p>
      </Panel>

      <BottomSheet
        open={sheetOpen}
        title="Create an API key"
        onClose={closeCreate}
        footer={!created && <Button variant="primary" onClick={() => void createKey()} disabled={!keyName.trim()}>Create key</Button>}
      >
        {created ? (
          <>
            <Banner tone="warn" title="Copy this key now">It is shown once and cannot be recovered.</Banner>
            <CopyRow label="API key" text={created.key} />
          </>
        ) : (
          <TextField label="Name" value={keyName} onChange={(e) => setKeyName(e.target.value)} hint="For example ci-deploy." />
        )}
      </BottomSheet>
    </div>
  );
}
