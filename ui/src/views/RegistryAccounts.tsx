import { useEffect, useRef, useState } from 'react';
import { ApiError, api } from '../api/client';
import type { RegistryAccount } from '../api/types';
import { Icon } from '../components/Icon';

/** Offered as suggestions, not a closed list: any v2 registry works, these
 *  are just the ones worth not making someone type. */
const KNOWN_REGISTRIES = ['docker.io', 'ghcr.io', 'quay.io', 'registry.gitlab.com'];

type Props = {
  accounts: RegistryAccount[];
  onClose: () => void;
  onChanged: () => void;
};

/**
 * Add, test and remove registry logins.
 *
 * Every save is checked against the registry first — the server refuses to
 * store a login it could not authenticate with. A wrong password that is
 * accepted quietly does not fail here; it fails later, on another node, as an
 * image that will not pull, which is a much worse place to find a typo.
 *
 * The password is write-only end to end: it is sealed with the cluster key,
 * and no route hands it back, so this form can show whether an account exists
 * and under what username, and never what its password is.
 */
export function RegistryAccounts({ accounts, onClose, onChanged }: Props) {
  const [server, setServer] = useState('docker.io');
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [showPassword, setShowPassword] = useState(false);
  const [busy, setBusy] = useState<'save' | 'test' | 'remove' | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [ok, setOk] = useState<string | null>(null);
  const dialogRef = useRef<HTMLDivElement>(null);

  // Escape closes, and focus starts inside the dialog rather than wherever
  // the page happened to leave it.
  useEffect(() => {
    dialogRef.current?.querySelector('input')?.focus();
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  function describe(e: unknown): string {
    return e instanceof ApiError ? e.message : String(e);
  }

  async function test() {
    setBusy('test');
    setError(null);
    setOk(null);
    try {
      await api.verifyRegistry({ server, username, password });
      setOk(`${server} accepted these credentials`);
    } catch (e) {
      setError(describe(e));
    } finally {
      setBusy(null);
    }
  }

  async function save() {
    setBusy('save');
    setError(null);
    setOk(null);
    try {
      await api.putRegistry({ server, username, password });
      setOk(`Saved credentials for ${server}`);
      setPassword('');
      onChanged();
    } catch (e) {
      setError(describe(e));
    } finally {
      setBusy(null);
    }
  }

  async function remove(target: string) {
    if (!confirm(`Remove stored login for ${target}?`)) return;
    setBusy('remove');
    setError(null);
    setOk(null);
    try {
      await api.removeRegistry(target);
      setOk(`Removed login for ${target}`);
      onChanged();
    } catch (e) {
      setError(describe(e));
    } finally {
      setBusy(null);
    }
  }

  const complete = server.trim() !== '' && username.trim() !== '' && password.trim() !== '';

  return (
    <div className="dialog-backdrop" onClick={onClose}>
      <div
        className="dialog"
        role="dialog"
        aria-modal="true"
        aria-label="Registry accounts"
        ref={dialogRef}
        onClick={(e) => e.stopPropagation()}
        style={{ width: 'min(640px, 100%)' }}
      >
        <div className="section-head">
          <h2>
            <Icon name="key" /> Registry accounts
          </h2>
          <button className="ghost icon-btn" aria-label="Close" onClick={onClose}>
            <Icon name="close" />
          </button>
        </div>

        <p className="page-sub" style={{ marginTop: '-4px' }}>
          Credentials used to pull private images and check for tag updates.
          Passwords are sealed with the cluster key and never sent back in plaintext.
        </p>

        {accounts.length > 0 ? (
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Registry</th>
                  <th>Username</th>
                  <th style={{ width: '40px' }} />
                </tr>
              </thead>
              <tbody>
                {accounts.map((a) => (
                  <tr key={a.server}>
                    <td className="mono" style={{ fontWeight: 550 }}>
                      {a.server}
                    </td>
                    <td>{a.username}</td>
                    <td style={{ textAlign: 'right' }}>
                      <button
                        className="ghost icon-btn"
                        title={`Remove ${a.server}`}
                        aria-label={`Remove ${a.server}`}
                        disabled={busy !== null}
                        onClick={() => void remove(a.server)}
                      >
                        <Icon name="trash" />
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <div
            style={{
              padding: 'var(--space-3)',
              background: 'var(--surface-2)',
              borderRadius: 'var(--radius-sm)',
              color: 'var(--text-dim)',
              fontSize: '13px',
              textAlign: 'center',
            }}
          >
            No registry credentials stored yet
          </div>
        )}

        <div style={{ marginTop: 'var(--space-2)', borderTop: '1px solid var(--border)', paddingTop: 'var(--space-3)' }}>
          <h3 style={{ fontSize: '14px', margin: '0 0 var(--space-2) 0', fontWeight: 600 }}>
            Add or update credentials
          </h3>

          <div style={{ display: 'flex', gap: '6px', flexWrap: 'wrap', marginBottom: 'var(--space-3)' }}>
            <span style={{ fontSize: '12px', color: 'var(--text-faint)', alignSelf: 'center', marginRight: '4px' }}>
              Quick select:
            </span>
            {KNOWN_REGISTRIES.map((r) => (
              <button
                key={r}
                type="button"
                className="ghost"
                style={{
                  padding: '2px 8px',
                  fontSize: '12px',
                  borderRadius: 'var(--radius-sm)',
                  background: server === r ? 'var(--surface-3)' : 'transparent',
                }}
                onClick={() => setServer(r)}
              >
                {r}
              </button>
            ))}
          </div>

          <div className="field">
            <label htmlFor="reg-server">Registry Host</label>
            <input
              id="reg-server"
              type="text"
              list="known-registries"
              value={server}
              onChange={(e) => setServer(e.target.value)}
              placeholder="e.g. docker.io, ghcr.io, or git.example.com:5000"
            />
            <datalist id="known-registries">
              {KNOWN_REGISTRIES.map((r) => (
                <option key={r} value={r} />
              ))}
            </datalist>
          </div>

          <div className="field">
            <label htmlFor="reg-username">Username</label>
            <input
              id="reg-username"
              type="text"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              placeholder="e.g. your-username or token-name"
              autoComplete="off"
            />
          </div>

          <div className="field">
            <label htmlFor="reg-password">Password or Personal Access Token</label>
            <div style={{ display: 'flex', gap: '8px', alignItems: 'center' }}>
              <input
                id="reg-password"
                type={showPassword ? 'text' : 'password'}
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                placeholder="Write-only token or password"
                autoComplete="new-password"
                style={{ flex: 1 }}
              />
              <button
                type="button"
                className="ghost icon-btn"
                title={showPassword ? 'Hide password' : 'Show password'}
                aria-label={showPassword ? 'Hide password' : 'Show password'}
                onClick={() => setShowPassword((v) => !v)}
              >
                <Icon name={showPassword ? 'eye-off' : 'eye'} />
              </button>
            </div>
            <span className="field-hint">
              Credentials are tested against the registry before being stored in the cluster.
            </span>
          </div>
        </div>

        {error && (
          <p className="banner banner-bad">
            <Icon name="alert-circle" /> {error}
          </p>
        )}
        {ok && (
          <p className="banner banner-good">
            <Icon name="check" /> {ok}
          </p>
        )}

        <div className="btn-row" style={{ marginTop: 'var(--space-2)' }}>
          <button
            type="button"
            className="ghost"
            disabled={!complete || busy !== null}
            onClick={() => void test()}
          >
            {busy === 'test' ? <span className="spinner" /> : <Icon name="check" />} Test connection
          </button>
          <button
            type="button"
            className="primary"
            disabled={!complete || busy !== null}
            onClick={() => void save()}
          >
            {busy === 'save' ? <span className="spinner" /> : <Icon name="upload" />} Save credentials
          </button>
        </div>
      </div>
    </div>
  );
}
