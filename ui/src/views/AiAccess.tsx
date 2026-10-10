// How to point an AI tool at this cluster over MCP, and which tools are
// already connected. Connecting itself happens in the tool: it discovers the
// OAuth endpoints from the MCP url and sends the user through the consent page.

import { useEffect, useState } from 'react';
import { api, ApiError } from '../api/client';
import type { McpClient } from '../api/types';
import { useAuth } from '../auth/AuthContext';
import { Badge } from '../components/Badge';
import { Icon } from '../components/Icon';

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
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard access can be denied; the text is selectable as a fallback.
    }
  };
  return (
    <div className="field">
      <label>{label}</label>
      <div className="flex-row" style={{ flexWrap: 'nowrap' }}>
        <pre style={{ flex: 1, margin: 0, overflowX: 'auto' }}>
          <code>{text}</code>
        </pre>
        <button type="button" className="ghost icon-btn" onClick={copy} aria-label={`Copy ${label}`}>
          <Icon name={copied ? 'check' : 'copy'} />
        </button>
      </div>
    </div>
  );
}

export function AiAccess() {
  const { isAdmin } = useAuth();
  const mcpUrl = `${window.location.origin}/mcp`;
  const [clients, setClients] = useState<McpClient[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = () =>
    api.listMcpClients().then(
      (c) => setClients(c),
      (e) => setError(e instanceof ApiError ? e.message : 'Could not load connected clients.'),
    );
  useEffect(() => {
    void load();
  }, []);

  const revoke = async (c: McpClient) => {
    if (!confirm(`Revoke ${c.client_name}? It loses access immediately.`)) return;
    try {
      await api.revokeMcpClient(c.client_id);
      await load();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Could not revoke that client.');
    }
  };

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="sparkle" />
          <div>
            <h1>AI access</h1>
            <p className="page-sub">Let AI tools inspect, and optionally manage, this cluster over MCP.</p>
          </div>
        </div>
      </div>

      <section className="card">
        <h2>Connect a tool</h2>
        <p className="muted">
          Each tool signs in through your browser and asks you to approve it, choosing read-only or admin access.
        </p>
        <CopyRow label="MCP URL" text={mcpUrl} />
        <CopyRow label="Claude Code" text={`claude mcp add --transport http ${SERVER_NAME} ${mcpUrl}`} />
        <p className="muted">
          The command only registers the server. Then start <code>claude</code>, run <code>/mcp</code>, pick{' '}
          {SERVER_NAME} and choose Authenticate to open the approval page.
        </p>
        <div className="flex-row">
          <a href="https://claude.ai/settings/connectors" target="_blank" rel="noreferrer">
            <button type="button">
              <Icon name="external" /> Claude.ai connectors
            </button>
          </a>
          <a href={cursorInstallLink(SERVER_NAME, mcpUrl)}>
            <button type="button">
              <Icon name="download" /> Add to Cursor
            </button>
          </a>
          <a href={vscodeInstallLink(SERVER_NAME, mcpUrl)}>
            <button type="button">
              <Icon name="download" /> Add to VS Code
            </button>
          </a>
        </div>
        <p className="muted">
          For claude.ai, choose Add custom connector and paste the MCP URL above. The URL must be reachable from the
          internet for the hosted app; Claude Code, Cursor and VS Code connect from your own machine.
        </p>
      </section>

      <section className="card">
        <h2>Connected clients</h2>
        {error && (
          <p className="field-error">
            <Icon name="alert-circle" /> {error}
          </p>
        )}
        {clients === null && !error && (
          <p className="loading">
            <span className="spinner" /> Loading...
          </p>
        )}
        {clients && clients.length === 0 && <p className="muted">No tool has been approved yet.</p>}
        {clients && clients.length > 0 && (
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Client</th>
                  <th>Access</th>
                  <th>Approved by</th>
                  <th>Since</th>
                  {isAdmin && <th />}
                </tr>
              </thead>
              <tbody>
                {clients.map((c) => (
                  <tr key={c.client_id}>
                    <td>{c.client_name}</td>
                    <td>
                      <Badge tone={c.grant === 'admin' ? 'warn' : 'neutral'}>
                        {c.grant === 'admin' ? 'Admin' : 'Read-only'}
                      </Badge>
                    </td>
                    <td>{c.approved_by}</td>
                    <td>{new Date(c.created_at * 1000).toLocaleDateString()}</td>
                    {isAdmin && (
                      <td>
                        <button type="button" className="danger" onClick={() => revoke(c)}>
                          <Icon name="trash" /> Revoke
                        </button>
                      </td>
                    )}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>
    </div>
  );
}
