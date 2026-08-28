import { useCallback, useEffect, useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import { ApiError, api } from '../api/client';
import type { BulkResult, RegistryAccount, UpdateRow } from '../api/types';
import { Badge } from '../components/Badge';
import { Icon } from '../components/Icon';
import { RegistryAccounts } from './RegistryAccounts';

/** Digests are 71 characters of noise; the first few are enough to compare
 *  two by eye, and the full value is on the title attribute. */
function shortDigest(digest: string | null): string {
  if (!digest) return '-';
  const hex = digest.startsWith('sha256:') ? digest.slice('sha256:'.length) : digest;
  return hex.slice(0, 12);
}

type Status = 'update' | 'drift' | 'current' | 'unknown';

/** One row's verdict, in the order that matters: an error means we do not
 *  know, and "we do not know" must never render as "up to date". */
function statusOf(row: UpdateRow): Status {
  if (row.error || !row.running_digest || !row.latest_digest) return 'unknown';
  if (row.update_available) return 'update';
  if (row.pin_drift) return 'drift';
  return 'current';
}

const STATUS_LABEL: Record<Status, string> = {
  update: 'update available',
  drift: 'not on its pin',
  current: 'up to date',
  unknown: 'unknown',
};

const STATUS_TONE: Record<Status, 'good' | 'warn' | 'bad' | 'neutral'> = {
  update: 'warn',
  drift: 'bad',
  current: 'good',
  unknown: 'neutral',
};

export function Updates() {
  const [rows, setRows] = useState<UpdateRow[] | null>(null);
  const [accounts, setAccounts] = useState<RegistryAccount[]>([]);
  const [accountsOpen, setAccountsOpen] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [results, setResults] = useState<BulkResult[] | null>(null);

  const load = useCallback(async () => {
    try {
      const list = await api.listUpdates();
      setRows(list);
      setError(null);
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e));
    }
  }, []);

  const loadAccounts = useCallback(async () => {
    try {
      setAccounts(await api.listRegistries());
    } catch {
      // A node with no cluster key cannot open the logins. That is worth
      // saying inside the accounts dialog, not as a page-level error on a
      // view whose main job has nothing to do with them.
      setAccounts([]);
    }
  }, []);

  // Not polled. Every load costs one registry round trip per distinct image
  // across the cluster, so this refreshes when asked and not on a timer.
  useEffect(() => {
    void load();
    void loadAccounts();
  }, [load, loadAccounts]);

  const outdated = useMemo(() => (rows ?? []).filter((r) => statusOf(r) === 'update'), [rows]);
  const unpinned = useMemo(() => (rows ?? []).filter((r) => !r.pinned_digest && r.running_digest), [rows]);

  async function run(label: string, action: () => Promise<BulkResult[]>) {
    setBusy(label);
    setError(null);
    setResults(null);
    try {
      const outcome = await action();
      setResults(outcome);
      await load();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  }

  if (!rows) {
    return (
      <p className="loading">
        <span className="spinner" /> Checking registries...
      </p>
    );
  }

  const failures = (results ?? []).filter((r) => !r.ok);

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="download" />
          <div>
            <h1>Updates</h1>
            <p className="page-sub">
              {outdated.length === 0
                ? `every container is on the newest image (${rows.length} checked)`
                : `${outdated.length} of ${rows.length} container(s) behind their registry`}
            </p>
          </div>
        </div>
        <div className="btn-row">
          <button className="ghost" onClick={() => setAccountsOpen(true)}>
            <Icon name="key" /> Registry accounts
            {accounts.length > 0 && ` (${accounts.length})`}
          </button>
          <button className="ghost" onClick={() => void load()} disabled={busy !== null}>
            <Icon name="refresh" /> Re-check
          </button>
          <button
            className="ghost"
            disabled={busy !== null || unpinned.length === 0}
            title="Record what each unpinned container is running as its pin, so it cannot drift on a later restart"
            onClick={() => void run('pin', () => api.pinUpdates())}
          >
            <Icon name="lock" /> Pin running ({unpinned.length})
          </button>
          <button
            className="primary"
            disabled={busy !== null || outdated.length === 0}
            onClick={() => void run('apply', () => api.applyUpdates())}
          >
            {busy === 'apply' ? <span className="spinner" /> : <Icon name="download" />} Update all (
            {outdated.length})
          </button>
        </div>
      </div>

      {error && <p className="banner banner-bad">{error}</p>}

      {results && (
        <div className="card" style={{ marginBottom: 'var(--space-4)' }}>
          <div className="section-head">
            <h2>
              <Icon name={failures.length === 0 ? 'check' : 'alert'} />{' '}
              {results.length === 0
                ? 'Nothing to do'
                : `${results.length - failures.length} of ${results.length} succeeded`}
            </h2>
            <button className="ghost icon-btn" aria-label="Dismiss" onClick={() => setResults(null)}>
              <Icon name="close" />
            </button>
          </div>
          {failures.length > 0 && (
            <ul className="muted">
              {failures.map((f) => (
                <li key={f.id}>
                  <span className="mono">{f.id}</span> — {f.error ?? 'failed'}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}

      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Instance</th>
              <th>Node</th>
              <th>Image</th>
              <th>Running</th>
              <th>Latest</th>
              <th>Pinned</th>
              <th>Status</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => {
              const status = statusOf(row);
              return (
                <tr key={row.id}>
                  <td>
                    <Link className="row-link" to={`/instances/${encodeURIComponent(row.id)}`}>
                      {row.id}
                    </Link>
                  </td>
                  <td>{row.node}</td>
                  <td className="mono">{row.image ?? '-'}</td>
                  <td className="mono" title={row.running_digest ?? undefined}>
                    {shortDigest(row.running_digest)}
                  </td>
                  <td className="mono" title={row.latest_digest ?? undefined}>
                    {shortDigest(row.latest_digest)}
                  </td>
                  <td className="mono" title={row.pinned_digest ?? undefined}>
                    {row.pinned_digest ? shortDigest(row.pinned_digest) : <span className="muted">unpinned</span>}
                  </td>
                  <td>
                    <Badge tone={STATUS_TONE[status]} title={row.error}>
                      {STATUS_LABEL[status]}
                    </Badge>
                  </td>
                  <td>
                    {status === 'update' && (
                      <button
                        className="ghost"
                        disabled={busy !== null}
                        onClick={() => void run(row.id, () => api.applyUpdates([row.id]))}
                      >
                        Update
                      </button>
                    )}
                  </td>
                </tr>
              );
            })}
            {rows.length === 0 && (
              <tr>
                <td colSpan={8} className="empty-cell">
                  nothing is running yet
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>

      {accountsOpen && (
        <RegistryAccounts
          accounts={accounts}
          onClose={() => setAccountsOpen(false)}
          onChanged={() => void loadAccounts()}
        />
      )}
    </div>
  );
}
