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
  const [busy, setBusy] = useState<'save' | 'test' | null>(null);
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
      setOk(`saved the login for ${server}`);
      setPassword('');
      onChanged();
    } catch (e) {
      setError(describe(e));
    } finally {
      setBusy(null);
    }
  }

  async function remove(target: string) {
    setError(null);
    setOk(null);
    try {
      await api.removeRegistry(target);
      onChanged();
    } catch (e) {
      setError(describe(e));
    }
  }

  const complete = server.trim() !== '' && username.trim() !== '' && password !== '';

  return (
    <div className="dialog-backdrop" onClick={onClose}>
      <div
        className="dialog"
        role="dialog"
        aria-modal="true"
        aria-label="Registry accounts"
        ref={dialogRef}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="section-head">
          <h2>
            <Icon name="key" /> Registry accounts
          </h2>
          <button className="ghost icon-btn" aria-label="Close" onClick={onClose}>
            <Icon name="close" />
          </button>
        </div>

        <p className="page-sub">
          Used to pull private images and to ask a registry what a tag currently points at. Stored as sealed
          cluster secrets; passwords are never readable back.
        </p>

        {accounts.length > 0 && (
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Registry</th>
                  <th>Username</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {accounts.map((a) => (
                  <tr key={a.server}>
                    <td className="mono">{a.server}</td>
                    <td>{a.username}</td>
                    <td>
                      <button className="ghost icon-btn" aria-label={`Remove ${a.server}`} onClick={() => void remove(a.server)}>
                        <Icon name="trash" />
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}

        <label>
          Registry
          <input
            list="known-registries"
            value={server}
            onChange={(e) => setServer(e.target.value)}
            placeholder="docker.io"
          />
          <datalist id="known-registries">
            {KNOWN_REGISTRIES.map((r) => (
              <option key={r} value={r} />
            ))}
          </datalist>
        </label>
        <label>
          Username
          <input value={username} onChange={(e) => setUsername(e.target.value)} autoComplete="off" />
        </label>
        <label>
          Password or access token
          <input
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            autoComplete="new-password"
          />
        </label>

        {error && <p className="banner banner-bad">{error}</p>}
        {ok && <p className="banner banner-good">{ok}</p>}

        <div className="btn-row">
          <button className="ghost" disabled={!complete || busy !== null} onClick={() => void test()}>
            {busy === 'test' ? <span className="spinner" /> : <Icon name="check" />} Test
          </button>
          <button className="primary" disabled={!complete || busy !== null} onClick={() => void save()}>
            {busy === 'save' ? <span className="spinner" /> : <Icon name="upload" />} Save
          </button>
        </div>
      </div>
    </div>
  );
}
