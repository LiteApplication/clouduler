import { useCallback, useEffect, useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import { ApiError, api } from '../api/client';
import type { BulkResult, InstanceState, RegistryAccount, UpdateRow } from '../api/types';
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

/** Rows needing action float to the top: an update available first, then a
 *  pin drift, then an unknown verdict, with already-current rows last. */
const STATUS_PRIORITY: Record<Status, number> = { update: 0, drift: 1, unknown: 2, current: 3 };

type UpdatePhase = 'pulling' | 'stopping' | 'starting' | 'healthy';

/** An update in flight goes through four real steps, in this order: the old
 *  container stays up throughout the pull, only coming down once the new
 *  bits are already on disk, and the new one is not "done" just because it
 *  started — its healthcheck (if it has one) still has to pass. */
const PHASE_ORDER: UpdatePhase[] = ['pulling', 'stopping', 'starting', 'healthy'];

const PHASE_LABEL: Record<UpdatePhase, string> = {
  pulling: 'downloading image',
  stopping: 'stopping old container',
  starting: 'starting new container',
  healthy: 'waiting for health check',
};

const PHASE_ICON: Record<UpdatePhase, 'download' | 'stop' | 'restart' | 'activity'> = {
  pulling: 'download',
  stopping: 'stop',
  starting: 'restart',
  healthy: 'activity',
};

/** Which of the four phases an instance is in right now, or `null` once
 *  there is nothing left to show. `reason` (from the SSE progress store,
 *  `/api/instances/events`) drives the first three phases in real time and
 *  comes from the reconciler's own redeploy step; the fourth has no
 *  reconciler step behind it at all — it is this view noticing, from the
 *  periodic `/api/instances` poll, that the new container is up but its
 *  docker healthcheck has not reported healthy yet. A container with no
 *  healthcheck configured skips straight past it. */
function phaseOf(reason: string | null, live: InstanceState | undefined): UpdatePhase | null {
  if (reason?.includes('pulling')) return 'pulling';
  if (reason?.includes('stopping')) return 'stopping';
  if (reason?.includes('starting')) return 'starting';
  if (live?.health === 'starting') return 'healthy';
  return null;
}

/** An instance an `apply` is watching counts as settled once it is back to a
 *  steady state (`running`/`stopped`) with no reconciler step in progress
 *  and, if it has a healthcheck, that check is no longer pending. Checking
 *  `reason` here (not just `state`) matters: during the pull step the old
 *  container is still up and reporting `running` the whole time, so a naive
 *  state check would call the row settled before the update even started. */
function isSettled(i: InstanceState): boolean {
  return (i.state === 'running' || i.state === 'stopped') && !i.reason && i.health !== 'starting';
}

/** One row's live update feedback: a four-segment progress bar over
 *  `PHASE_ORDER`, kept apart so only this cell re-renders on every SSE
 *  `progress` event rather than the whole table. `live` (the instance's most
 *  recent `/api/instances` poll) supplies the healthcheck phase SSE cannot. */
function UpdateProgressBar({ id, live }: { id: string; live: InstanceState | undefined }) {
  const progress = useInstanceProgress(id);
  const phase = phaseOf(progress?.reason ?? null, live);
  if (!phase) {
    return (
      <span className="muted">
        <span className="spinner" /> starting
      </span>
    );
  }
  const stepIndex = PHASE_ORDER.indexOf(phase);
  return (
    <div className="update-progress">
      <div className="update-progress-head">
        <span className="spinner" />
        <Icon name={PHASE_ICON[phase]} />
        <span>{PHASE_LABEL[phase]}</span>
        <span className="mono faint">
          ({stepIndex + 1}/{PHASE_ORDER.length})
        </span>
      </div>
      <div className="update-progress-track" aria-hidden="true">
        {PHASE_ORDER.map((p, i) => (
          <span
            key={p}
            className={`update-progress-seg${i < stepIndex ? ' done' : ''}${i === stepIndex ? ' active' : ''}`}
          />
        ))}
      </div>
    </div>
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
   *  shown per row comes from the SSE-driven progress store (`UpdateProgressBar`);
   *  this only tracks which instances to keep watching and when to stop. */
  const [pending, setPending] = useState<string[]>([]);
  /** Each pending instance's most recent `/api/instances` poll, so
   *  `UpdateProgressBar` can show the healthcheck phase SSE alone cannot. */
  const [live, setLive] = useState<Record<string, InstanceState>>({});

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
  // they settle — the per-row phase (downloading/stopping/starting/health
  // check) comes from the SSE progress store plus this poll's `health`
  // field (`UpdateProgressBar`), but nothing there says "done"; that still
  // has to come from a live docker inspect. `/api/instances` is cheap enough
  // for a 3s poll, unlike `/api/updates`. When every accepted instance has
  // settled (see `isSettled` — not just "running again": the old container
  // reports running throughout the pull too, so that alone is not "done"),
  // re-check digests once so the rows go green on their own.
  useEffect(() => {
    if (pending.length === 0) return;
    let cancelled = false;
    const tick = async () => {
      try {
        const instances = await api.listInstances();
        if (cancelled) return;
        const watched = instances.filter((i) => pending.includes(i.id));
        setLive((prev) => {
          const next = { ...prev };
          for (const i of watched) next[i.id] = i;
          return next;
        });
        if (watched.length > 0 && watched.every(isSettled)) {
          setPending([]);
          setLive({});
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
  // Needs-an-update rows first, so the table opens on what there is to act
  // on rather than making it scroll past everything already current.
  const sortedRows = useMemo(() => {
    return [...(rows ?? [])].sort((a, b) => {
      const priority = STATUS_PRIORITY[statusOf(a)] - STATUS_PRIORITY[statusOf(b)];
      return priority !== 0 ? priority : a.id.localeCompare(b.id);
    });
  }, [rows]);

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
   * phase comes from `/api/instances/events` (`UpdateProgressBar`), with
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
            {sortedRows.map((row) => {
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
                      <UpdateProgressBar id={row.id} live={live[row.id]} />
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
