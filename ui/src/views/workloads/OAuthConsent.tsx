// Approval screen for an MCP client that sent the browser here through
// /oauth/authorize. Rendered outside the app shell: it is a one-question page
// the user reaches from another application and leaves again straight away.
// Not being logged in is handled by App, which shows the login screen at this
// same URL, so signing in lands back here (API key) or via the `return_to`
// the OIDC login carries (OIDC).

import { useEffect, useState } from 'react';
import { api, ApiError } from '../../api/client';
import type { McpGrant, OAuthRequestInfo } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Logo } from '../../components/Logo';
import { Banner, Button, Panel } from '../../ds';
import './workloads.css';

const GRANT_LABELS: Record<McpGrant, { title: string; detail: string }> = {
  read: {
    title: 'Read-only',
    detail: 'Can inspect nodes, stacks, containers and logs. Secret values stay redacted. Cannot change anything.',
  },
  admin: {
    title: 'Admin',
    detail: 'Can also change the cluster: deploy and edit stacks, restart or move containers, and manage nodes.',
  },
};

export function OAuthConsent() {
  const requestId = new URLSearchParams(window.location.search).get('req');
  const { identity } = useAuth();
  const [info, setInfo] = useState<OAuthRequestInfo | null>(null);
  const [grant, setGrant] = useState<McpGrant>('read');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<'approve' | 'deny' | null>(null);

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
            : 'Could not load this request. Try opening it again from your application.',
        ),
    );
  }, [requestId]);

  const finish = async (kind: 'approve' | 'deny', action: () => Promise<{ redirect: string }>) => {
    setBusy(kind);
    setError(null);
    try {
      const { redirect } = await action();
      window.location.assign(redirect);
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Something went wrong. Try again.');
      setBusy(null);
    }
  };

  return (
    <div className="wl-login wl-login--solo">
      <main className="wl-login__main">
        <div className="wl-login__col">
          <div className="col">
            <span className="wl-login__brand">
              <Logo size={48} />
              clouduler
            </span>
            <h1 className="wl-login__title wl-login__title--sm">Connect an application</h1>
            {identity?.username && <p className="sub">{`Signed in as ${identity.username}`}</p>}
          </div>

          {error && (
            <Banner tone="danger" title="Cannot continue">
              {error}
            </Banner>
          )}

          {info && requestId && (
            <Panel>
              <p>
                <strong>{info.client_name}</strong> wants to access this cluster on your behalf. After you approve, it returns to{' '}
                <code>{info.redirect_host}</code>.
              </p>

              <fieldset className="wl-grants" disabled={busy !== null}>
                <legend className="eyebrow">What may it do?</legend>
                {(['read', 'admin'] as McpGrant[])
                  .filter((g) => info.allowed_grants.includes(g))
                  .map((g) => (
                    <label key={g} className={`wl-grant${grant === g ? ' wl-grant--on' : ''}`}>
                      <input type="radio" name="grant" checked={grant === g} onChange={() => setGrant(g)} />
                      <span>
                        <strong>{GRANT_LABELS[g].title}</strong>
                        <br />
                        <span className="sub">{GRANT_LABELS[g].detail}</span>
                      </span>
                    </label>
                  ))}
              </fieldset>

              <div className="acts">
                <Button variant="primary" icon="check" loading={busy === 'approve'} disabled={busy !== null} onClick={() => finish('approve', () => api.approveOAuthRequest(requestId, grant))}>
                  Approve access
                </Button>
                <Button icon="close" loading={busy === 'deny'} disabled={busy !== null} onClick={() => finish('deny', () => api.denyOAuthRequest(requestId))}>
                  Deny access
                </Button>
              </div>
              <p className="sub">You can revoke access at any time from the AI access page.</p>
            </Panel>
          )}

          {!info && !error && (
            <p className="sub" role="status">
              Loading the request...
            </p>
          )}
        </div>
      </main>
    </div>
  );
}
