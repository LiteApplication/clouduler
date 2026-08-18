import { useState } from 'react';
import { useAuth } from '../auth/AuthContext';
import { api } from '../api/client';
import { Icon } from '../components/Icon';
import { Logo } from '../components/Logo';
import { useTheme } from '../theme';

// The key is traded once for a session cookie rather than kept around and
// attached per request. EventSource cannot send an Authorization header, so a
// key held client side would leave every SSE stream (/api/events, logs, stats)
// unauthenticated; the cookie the browser attaches on its own covers them.

export function Login() {
  const [key, setKey] = useState('');
  const [show, setShow] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const { refresh } = useAuth();
  // Mounted before the app shell, so the login screen is where the saved
  // theme first gets applied.
  useTheme();

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      await api.loginWithKey(key.trim());
      await refresh();
    } catch {
      setError('could not verify that key');
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="login">
      <div className="login-card">
        <div className="login-brand">
          <Logo size={64} className="brand-mark" />
          <div>
            <h1>clouduler</h1>
            <p className="muted" style={{ margin: 0 }}>
              Sign in to manage the cluster.
            </p>
          </div>
        </div>

        <a href="/auth/oidc/login" className="block">
          <button type="button" className="primary block">
            <Icon name="lock" /> Sign in with OIDC
          </button>
        </a>

        <div className="divider">or use an API key</div>

        <form onSubmit={submit}>
          <div className="field">
            <label htmlFor="api-key">API key</label>
            <div className="flex-row" style={{ flexWrap: 'nowrap' }}>
              <input
                id="api-key"
                type={show ? 'text' : 'password'}
                value={key}
                onChange={(e) => setKey(e.target.value)}
                placeholder="clk_..."
                autoComplete="off"
                spellCheck={false}
              />
              <button
                type="button"
                className="ghost icon-btn"
                onClick={() => setShow((v) => !v)}
                aria-label={show ? 'Hide key' : 'Show key'}
              >
                <Icon name={show ? 'eye-off' : 'eye'} />
              </button>
            </div>
          </div>
          {error && (
            <p className="field-error">
              <Icon name="alert-circle" /> {error}
            </p>
          )}
          <button type="submit" className="block" disabled={busy || key.trim().length === 0}>
            {busy ? <span className="spinner" /> : <Icon name="chevron-right" />}
            {busy ? 'Checking...' : 'Continue'}
          </button>
        </form>
      </div>
    </div>
  );
}
