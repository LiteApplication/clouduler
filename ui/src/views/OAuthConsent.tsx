// Approval screen for an MCP client that sent the browser here through
// /oauth/authorize. Rendered outside the app shell: it is a one-question page
// the user reaches from another application and leaves again straight away.
// Not being logged in is handled by App, which shows the login screen at this
// same URL, so signing in lands back here (API key) or via the `return_to`
// the OIDC login carries (OIDC).

import { useEffect, useState } from 'react';
import { api, ApiError } from '../api/client';
import type { McpGrant, OAuthRequestInfo } from '../api/types';
import { useAuth } from '../auth/AuthContext';
import { Icon } from '../components/Icon';
import { Logo } from '../components/Logo';

const GRANT_LABELS: Record<McpGrant, { title: string; detail: string }> = {
  read: {
    title: 'Read-only',
    detail: 'Can inspect nodes, stacks, instances and logs. Secret values stay redacted. Cannot change anything.',
  },
  admin: {
    title: 'Admin',
    detail: 'Can also change the cluster: deploy and edit stacks, restart or move instances, and manage nodes.',
  },
};

export function OAuthConsent() {
  const requestId = new URLSearchParams(window.location.search).get('req');
  const { identity } = useAuth();
  const [info, setInfo] = useState<OAuthRequestInfo | null>(null);
  const [grant, setGrant] = useState<McpGrant>('read');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!requestId) {
      setError('This page needs to be opened from an application asking for access.');
      return;
    }
    api.getOAuthRequest(requestId).then(
      (i) => {
        setInfo(i);
        // Default to what the client asked for, but never above what this user may give.
        setGrant(i.allowed_grants.includes(i.requested_grant) ? i.requested_grant : 'read');
      },
      (e) =>
        setError(
          e instanceof ApiError && e.status === 404
            ? 'This request expired or is no longer available. Start the connection again from your application.'
            : 'Could not load this request.',
        ),
    );
  }, [requestId]);

  const finish = async (action: () => Promise<{ redirect: string }>) => {
    setBusy(true);
    setError(null);
    try {
      const { redirect } = await action();
      window.location.assign(redirect);
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Something went wrong, try again.');
      setBusy(false);
    }
  };

  return (
    <div className="login">
      <div className="login-card">
        <div className="login-brand">
          <Logo size={64} className="brand-mark" />
          <div>
            <h1>Connect an application</h1>
            <p className="muted" style={{ margin: 0 }}>
              Signed in as {identity?.username}
            </p>
          </div>
        </div>

        {error && (
          <p className="field-error">
            <Icon name="alert-circle" /> {error}
          </p>
        )}

        {info && requestId && (
          <>
            <p>
              <strong>{info.client_name}</strong> wants to access this cluster on your behalf. After you approve, it
              returns to <code>{info.redirect_host}</code>.
            </p>

            <fieldset className="field" style={{ border: 0, padding: 0 }} disabled={busy}>
              <legend>What may it do?</legend>
              {(['read', 'admin'] as McpGrant[])
                .filter((g) => info.allowed_grants.includes(g))
                .map((g) => (
                  <label key={g} className="flex-row" style={{ alignItems: 'flex-start', marginBottom: 8 }}>
                    <input type="radio" name="grant" checked={grant === g} onChange={() => setGrant(g)} />
                    <span>
                      <strong>{GRANT_LABELS[g].title}</strong>
                      <br />
                      <span className="muted">{GRANT_LABELS[g].detail}</span>
                    </span>
                  </label>
                ))}
            </fieldset>

            <div className="btn-row">
              <button
                type="button"
                className="primary"
                disabled={busy}
                onClick={() => finish(() => api.approveOAuthRequest(requestId, grant))}
              >
                {busy ? <span className="spinner" /> : <Icon name="check" />} Approve
              </button>
              <button type="button" disabled={busy} onClick={() => finish(() => api.denyOAuthRequest(requestId))}>
                <Icon name="close" /> Deny
              </button>
            </div>
            <p className="muted">You can revoke access at any time from the AI access page.</p>
          </>
        )}

        {!info && !error && (
          <p className="loading">
            <span className="spinner" /> Loading...
          </p>
        )}
      </div>
    </div>
  );
}
