// Settings > Service login: the client addresses visitors saved at the login
// ("remember this IP address"), for an admin to review and revoke. Listed live
// from the server, not part of the config draft: these are not stored in
// cluster.yaml, secrets or the git backup.

import { useEffect, useState } from 'react';
import { api, ApiError } from '../../api/client';
import type { RememberedIp } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { useConfirm } from '../../ds';

export function RememberedIps() {
  const { isAdmin } = useAuth();
  const confirm = useConfirm();
  const [rows, setRows] = useState<RememberedIp[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!isAdmin) return;
    api.rememberedIps().then(setRows).catch(() => setRows([]));
  }, [isAdmin]);

  if (!isAdmin) return null;

  const revoke = async (row: RememberedIp) => {
    const ok = await confirm({
      title: `Forget ${row.ip}?`,
      body: `${row.username} will have to log in again from this address on ${row.network}.`,
      confirmLabel: 'Forget address',
      confirmVariant: 'danger',
    });
    if (!ok) return;
    setError(null);
    try {
      await api.forgetRememberedIp(row.id);
      setRows((rs) => (rs ?? []).filter((r) => r.id !== row.id));
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Could not forget the address.');
    }
  };

  return (
    <div className="field" data-testid="remembered-ips">
      <label>Saved IP addresses</label>
      <span className="field-hint">
        Addresses that skip the login. They expire on their own and are not part of the config or the git backup.
      </span>
      {error && <span className="field-error">{error}</span>}
      {rows && rows.length === 0 && <p className="muted">None saved.</p>}
      {(rows ?? []).map((r) => (
        <div className="btn-row" key={r.id}>
          <span style={{ flex: 1 }}>
            <code>{r.ip}</code> on {r.network}, {r.username}
            <span className="muted"> until {new Date(r.expires_at * 1000).toLocaleString()}</span>
          </span>
          <button type="button" onClick={() => void revoke(r)}>
            Forget
          </button>
        </div>
      ))}
    </div>
  );
}
