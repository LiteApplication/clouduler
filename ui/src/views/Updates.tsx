import { useCallback, useEffect, useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import { ApiError, api } from '../api/client';
import type { BulkResult, RegistryAccount, UpdateRow } from '../api/types';
import { Badge } from '../components/Badge';
import { Icon } from '../components/Icon';
import { useInstanceProgress } from '../state/progressStore';
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

/** An update in flight goes through three real docker steps, in this order:
 *  the old container stays up throughout the pull, only coming down once the
 *  new bits are already on disk. `reason` (from `/api/instances/events`)
 *  spells out which one a redeploying instance is on, e.g.
 *  `"redeploying (pulling, step 1/3)"` — pull the phase word out of it so the
 *  row can show something a lot clearer than that raw string. */
function updatePhase(reason: string | null): { label: string; icon: 'download' | 'stop' | 'restart' } | null {
  if (!reason) return null;
  if (reason.includes('pulling')) return { label: 'downloading image', icon: 'download' };
  if (reason.includes('stopping')) return { label: 'stopping old container', icon: 'stop' };
  if (reason.includes('starting')) return { label: 'starting new container', icon: 'restart' };
  return { label: reason, icon: 'restart' };
}

/** One row's live update feedback: the phase an in-flight update is on, kept
 *  apart so only this cell re-renders on every `progress` event rather than
 *  the whole table. */
function UpdateProgressCell({ id, fallback }: { id: string; fallback: string }) {
  const progress = useInstanceProgress(id);
  const phase = updatePhase(progress?.reason ?? null);
  if (!phase) {
    return (
      <span className="muted">
        <span className="spinner" /> {fallback}
      </span>
    );
  }
  return (
    <span className="update-progress">
      <span className="spinner" />
      <Icon name={phase.icon} />
      {phase.label}
      {progress?.step && (
        <span className="mono faint">
          {' '}
          ({progress.step[0]}/{progress.step[1]})
        </span>
      )}
    </span>
  );
}

export function Updates() {
  const [rows, setRows] = useState<UpdateRow[] | null>(null);
  const [accounts, setAccounts] = useState<RegistryAccount[]>([]);
  const [accountsOpen, setAccountsOpen] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [results, setResults] = useState<BulkResult[] | null>(null);
  /** Instances an `apply` accepted and that have not settled yet. The phase
   *  shown per row comes from the SSE-driven progress store (`UpdateProgressCell`);
   *  this only tracks which instances to keep watching and when to stop. */
  const [pending, setPending] = useState<string[]>([]);

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

  // While updates are in flight, watch the instances themselves for when
  // they settle — the per-row phase (downloading/stopping/starting) comes
  // from the SSE progress store instead (`UpdateProgressCell`), but nothing
  // there says "done"; that still has to come from a live docker inspect.
  // `/api/instances` is cheap enough for a 3s poll, unlike `/api/updates`.
  // When every accepted instance is running again, re-check digests once so
  // the rows go green on their own.
  useEffect(() => {
    if (pending.length === 0) return;
    let cancelled = false;
    const tick = async () => {
      try {
        const instances = await api.listInstances();
        if (cancelled) return;
        const watched = instances.filter((i) => pending.includes(i.id));
        // `stopped` counts as settled too: an update that failed to come back
        // must end the watch rather than spin forever.
        if (watched.length > 0 && watched.every((i) => i.state === 'running' || i.state === 'stopped')) {
          setPending([]);
          void load();
        }
      } catch {
        // A failed poll is not worth surfacing; the next tick retries.
      }
    };
    void tick();
    const id = setInterval(() => void tick(), 3000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [pending, load]);

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

  /**
   * Start updates and follow them through the instance list.
   *
   * The server answers as soon as the work is scheduled, because a pull takes
   * far longer than a request may. So the ids are always sent explicitly —
   * this view is looking at the answer already, and letting the server
   * recompute it would make it re-scan every registry first — and per-row
   * phase comes from `/api/instances/events` (`UpdateProgressCell`), with
   * `/api/instances` polled only to notice when the batch has settled.
   */
  async function startUpdate(ids: string[]) {
    setBusy(ids.length === 1 ? ids[0] : 'apply');
    setError(null);
    setResults(null);
    try {
      const { accepted } = await api.applyUpdates(ids);
      setPending(accepted);
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
            disabled={busy !== null || pending.length > 0 || outdated.length === 0}
            onClick={() => void startUpdate(outdated.map((r) => r.id))}
          >
            {busy === 'apply' ? <span className="spinner" /> : <Icon name="download" />} Update all (
            {outdated.length})
          </button>
        </div>
      </div>

      {error && <p className="banner banner-bad">{error}</p>}

      {pending.length > 0 && (
        <p className="banner banner-warn">
          <span className="spinner" /> Updating {pending.length} instance(s): each downloads its new image first,
          while the old container keeps serving traffic, then restarts onto it once the download lands. Watch
          progress per instance below — this keeps going whether or not you stay on this page.
        </p>
      )}

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
              <th>Running</th>
              <th>Latest</th>
              <th>Pinned</th>
              <th>Status</th>
              <th />
              <th>Node</th>
              <th>Image</th>
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
                    {pending.includes(row.id) ? (
                      <UpdateProgressCell id={row.id} fallback="starting" />
                    ) : (
                      status === 'update' && (
                        <button
                          className="ghost"
                          disabled={busy !== null || pending.length > 0}
                          onClick={() => void startUpdate([row.id])}
                        >
                          <Icon name="download" /> Update
                        </button>
                      )
                    )}
                  </td>
                  <td>{row.node}</td>
                  <td className="mono">{row.image ?? '-'}</td>
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
