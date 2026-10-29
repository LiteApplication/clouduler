import { useState } from 'react';
import { api } from '../../api/client';
import { Banner, Button, TextField } from '../../ds';
import { Logo } from '../../components/Logo';
import { useAuth } from '../../auth/AuthContext';
import { useTheme } from '../../theme';
import './workloads.css';

// The key is traded once for a session cookie rather than kept around and
// attached per request. EventSource cannot send an Authorization header, so a
// key held client side would leave every SSE stream (/api/events, logs, stats)
// unauthenticated; the cookie the browser attaches on its own covers them.

export function Login() {
  const [key, setKey] = useState('');
  const [error, setError] = useState(false);
  const [busy, setBusy] = useState(false);
  const { refresh } = useAuth();
  // Mounted before the app shell, so the login screen is where the saved
  // theme first gets applied.
  useTheme();

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(false);
    setBusy(true);
    try {
      await api.loginWithKey(key.trim());
      await refresh();
    } catch {
      setError(true);
    } finally {
      setBusy(false);
    }
  };

  const returnTo = encodeURIComponent(window.location.pathname + window.location.search);

  return (
    <div className="wl-login">
      <section className="wl-login__art" aria-hidden="true">
        <div className="wl-login__shapes">
          <div className="disc wl-shape wl-shape--brand" />
          <div className="disc wl-shape wl-shape--pop" />
          <div className="wl-shape wl-shape--block" />
          <div className="disc wl-shape wl-shape--ring" />
        </div>
      </section>
      <main className="wl-login__main">
        <div className="wl-login__col">
          <div className="col">
            <span className="wl-login__brand">
              <Logo size={48} />
              clouduler
            </span>
            <h1 className="wl-login__title">Homelab, in one place</h1>
            <p className="sub">Sign in to see your stacks, your nodes and what needs you.</p>
          </div>

          <a className="cl-btn cl-btn--primary wl-block" href={`/auth/oidc/login?return_to=${returnTo}`}>
            <span>Sign in with your identity provider</span>
          </a>

          <div className="row wl-or" aria-hidden="true">
            <span />
            <span className="eyebrow">or use an API key</span>
            <span />
          </div>

          <form className="col" onSubmit={submit}>
            <TextField
              label="API key"
              type="password"
              mono
              value={key}
              placeholder="clk_..."
              hint="Create one under Settings, Access."
              onChange={(e) => setKey(e.target.value)}
            />
            <Button type="submit" className="wl-block" loading={busy} disabled={key.trim().length === 0}>
              Sign in with API key
            </Button>
          </form>

          {error && (
            <Banner tone="danger" title="That key was not accepted">
              Check that you copied all of it. Keys are shown once, so create a new one if it is lost.
            </Banner>
          )}
        </div>
      </main>
    </div>
  );
}
