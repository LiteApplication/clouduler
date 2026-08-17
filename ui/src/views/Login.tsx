import { useState } from 'react';
import { useAuth } from '../auth/AuthContext';

// There's no dedicated api-key login endpoint: a key is sent as a bearer
// token on every request. We stash it in sessionStorage and attach it via
// a fetch patch so the rest of the app doesn't need to know it exists.
const KEY_STORAGE = 'clouduler_api_key';

export function installStoredApiKey() {
  const key = sessionStorage.getItem(KEY_STORAGE);
  if (!key) return;
  const originalFetch = window.fetch.bind(window);
  window.fetch = (input, init) => {
    const headers = new Headers(init?.headers);
    if (!headers.has('Authorization')) headers.set('Authorization', `Bearer ${key}`);
    return originalFetch(input, { ...init, headers });
  };
}

export function Login() {
  const [key, setKey] = useState('');
  const [error, setError] = useState<string | null>(null);
  const { refresh } = useAuth();

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    sessionStorage.setItem(KEY_STORAGE, key.trim());
    installStoredApiKey();
    try {
      await refresh();
    } catch {
      setError('could not verify that key');
    }
  };

  return (
    <div style={{ display: 'grid', placeItems: 'center', minHeight: '100vh' }}>
      <div className="card" style={{ width: 380 }}>
        <h1>clouduler</h1>
        <p className="muted">Sign in to manage the cluster.</p>
        <a href="/auth/oidc/login">
          <button type="button" className="primary" style={{ width: '100%', marginBottom: 16 }}>
            Sign in with OIDC
          </button>
        </a>
        <p className="muted" style={{ fontSize: 12 }}>
          Or use an API key
        </p>
        <form onSubmit={submit}>
          <div className="field">
            <label htmlFor="api-key">API key</label>
            <input
              id="api-key"
              type="password"
              value={key}
              onChange={(e) => setKey(e.target.value)}
              placeholder="clk_..."
              autoComplete="off"
            />
          </div>
          {error && <p className="field-error">{error}</p>}
          <button type="submit" style={{ width: '100%' }} disabled={key.trim().length === 0}>
            Continue
          </button>
        </form>
      </div>
    </div>
  );
}
