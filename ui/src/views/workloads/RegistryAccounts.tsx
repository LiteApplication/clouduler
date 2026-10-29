import { useState } from 'react';
import { ApiError, api } from '../../api/client';
import type { RegistryAccount } from '../../api/types';
import { Banner, BottomSheet, Button, Chip, TextField, useConfirm } from '../../ds';
import './workloads.css';

/** Offered as suggestions, not a closed list: any v2 registry works. */
const KNOWN_REGISTRIES = ['docker.io', 'ghcr.io', 'quay.io', 'registry.gitlab.com'];

/**
 * Add, test and remove registry logins.
 *
 * Every save is checked against the registry first: the server refuses to
 * store a login it could not authenticate with, because a typo accepted
 * quietly only shows up later as an image that will not pull on another node.
 * The password is write-only end to end: sealed with the cluster key, and no
 * route hands it back.
 */
export function RegistryAccounts({
  open,
  accounts,
  onClose,
  onChanged,
}: {
  open: boolean;
  accounts: RegistryAccount[];
  onClose: () => void;
  onChanged: () => void;
}) {
  const [server, setServer] = useState('docker.io');
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [busy, setBusy] = useState<'save' | 'test' | 'remove' | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [ok, setOk] = useState<string | null>(null);
  const confirm = useConfirm();

  const describe = (e: unknown) => (e instanceof ApiError ? e.message : String(e));

  const attempt = async (kind: 'save' | 'test', action: () => Promise<unknown>, done: string, after?: () => void) => {
    setBusy(kind);
    setError(null);
    setOk(null);
    try {
      await action();
      setOk(done);
      after?.();
    } catch (e) {
      setError(describe(e));
    } finally {
      setBusy(null);
    }
  };

  const remove = async (target: string) => {
    if (!(await confirm({ title: `Remove the login for ${target}?`, body: 'Images from this registry may stop pulling.', confirmLabel: `Remove ${target}`, confirmVariant: 'danger' }))) return;
    setBusy('remove');
    setError(null);
    setOk(null);
    try {
      await api.removeRegistry(target);
      setOk(`Removed the login for ${target}`);
      onChanged();
    } catch (e) {
      setError(describe(e));
    } finally {
      setBusy(null);
    }
  };

  const complete = server.trim() !== '' && username.trim() !== '' && password.trim() !== '';
  const creds = { server, username, password };

  return (
    <BottomSheet open={open} title="Registry accounts" onClose={onClose}>
      <div className="sec">
        <p className="sub">Credentials used to pull private images and check for newer ones. Passwords are sealed with the cluster key and never sent back.</p>

        {accounts.length > 0 ? (
          <div className="list">
            {accounts.map((a) => (
              <div className="li" key={a.server}>
                <div className="li__main">
                  <span className="li__name mono">{a.server}</span>
                  <span className="li__meta">{a.username}</span>
                </div>
                <Button iconOnly icon="trash" label={`Remove ${a.server}`} variant="ghost" disabled={busy !== null} onClick={() => void remove(a.server)} />
              </div>
            ))}
          </div>
        ) : (
          <p className="sub">No registry logins stored yet.</p>
        )}

        <h3 className="h3">Add or update a login</h3>
        <div className="chips" role="group" aria-label="Quick select a registry">
          {KNOWN_REGISTRIES.map((r) => (
            <Chip key={r} label={r} selected={server === r} onClick={() => setServer(r)} />
          ))}
        </div>
        <TextField label="Registry host" mono value={server} onChange={(e) => setServer(e.target.value)} placeholder="docker.io, ghcr.io or git.example.com:5000" />
        <TextField label="Username" value={username} onChange={(e) => setUsername(e.target.value)} placeholder="Your username or token name" />
        <TextField
          label="Password or access token"
          type="password"
          autoComplete="new-password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          hint="Checked against the registry before it is stored."
        />

        {error && <Banner tone="danger" title="That did not work">{error}</Banner>}
        {ok && <Banner tone="ok" title={ok} />}

        <div className="acts">
          <Button icon="check" disabled={!complete || busy !== null} loading={busy === 'test'} onClick={() => void attempt('test', () => api.verifyRegistry(creds), `${server} accepted these credentials`)}>
            Test connection
          </Button>
          <Button
            variant="primary"
            icon="upload"
            disabled={!complete || busy !== null}
            loading={busy === 'save'}
            onClick={() => void attempt('save', () => api.putRegistry(creds), `Saved the login for ${server}`, () => (setPassword(''), onChanged()))}
          >
            Save login
          </Button>
        </div>
      </div>
    </BottomSheet>
  );
}
