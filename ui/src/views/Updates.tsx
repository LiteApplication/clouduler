import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { ApiError, api } from '../api/client';
import type { BulkResult, InstanceState, InstanceUpdate, RegistryAccount, UpdateRow } from '../api/types';
import { Badge } from '../components/Badge';
import { Icon } from '../components/Icon';
import { UPDATE_PHASE_LABEL, UPDATE_STEPS, isUpdateActive, type UpdateStep } from '../lib/updates';
import { useClusterState } from '../state/store';
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

const STEP_ICON: Record<UpdateStep, 'download' | 'stop' | 'restart' | 'activity'> = {
  pulling: 'download',
  stopping: 'stop',
  starting: 'restart',
  started: 'activity',
};

/** How long after its new container started a row keeps watching its
 *  healthcheck, via the `/api/instances` poll. */
const HEALTH_WATCH_SECS = 180;

/** One row's live update progress, read from the replicated update record
 *  (`ClusterState.instance_updates`), which the node doing the work writes
 *  at every real step — so it is the same no matter which node this browser
 *  is talking to. `update` is absent only in the instant between the server
 *  accepting the request and its queued record reaching this browser.
 *  `health` comes from the `/api/instances` poll: once the new container is
 *  started, its healthcheck (if it has one) still has to pass. */
function UpdateProgressBar({ update, health }: { update: InstanceUpdate | undefined; health: string | undefined }) {
  const phase = update?.phase ?? 'queued';
  if (phase === 'queued' || phase === 'failed') {
    return (
      <span className="muted">
        <span className="spinner" /> queued
      </span>
    );
  }
  const stepIndex = UPDATE_STEPS.indexOf(phase);
  const waitingForHealth = phase === 'started' && health === 'starting';
  return (
    <div className="update-progress">
      <div className="update-progress-head">
        <span className="spinner" />
        <Icon name={STEP_ICON[phase]} />
        <span>{waitingForHealth ? 'started, waiting for health check' : UPDATE_PHASE_LABEL[phase]}</span>
        <span className="mono faint">
          ({stepIndex + 1}/{UPDATE_STEPS.length})
        </span>
      </div>
      <div className="update-progress-track" aria-hidden="true">
        {UPDATE_STEPS.map((p, i) => (
          <span
            key={p}
            className={`update-progress-seg${i < stepIndex ? ' done' : ''}${i === stepIndex ? ' active' : ''}`}
          />
        ))}
      </div>
    </div>
  );
}

function nowSecs(): number {
  return Math.floor(Date.now() / 1000);
}

export function Updates() {
  const [rows, setRows] = useState<UpdateRow[] | null>(null);
  const [accounts, setAccounts] = useState<RegistryAccount[]>([]);
  const [accountsOpen, setAccountsOpen] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [results, setResults] = useState<BulkResult[] | null>(null);
  const cluster = useClusterState();
  const updates = useMemo(() => cluster?.instance_updates ?? {}, [cluster]);
  /** Instances an `apply` accepted whose queued record has not reached this
   *  browser yet, so a row never flickers back to its button in between. */
  const [requested, setRequested] = useState<string[]>([]);
  /** Recently started instances' latest `/api/instances` poll, for the
   *  healthcheck that follows the last update step. */
  const [live, setLive] = useState<Record<string, InstanceState>>({});

  // Everything below is derived from the replicated record, so an update
  // started from another browser, the cli, or before this page was opened
  // shows up here all the same.
  const active = useMemo(
    () => Object.keys(updates).filter((id) => isUpdateActive(updates[id])),
    [updates],
  );
  const inFlight = useMemo(() => new Set([...active, ...requested]), [active, requested]);
  const healthWatch = useMemo(
    () =>
      Object.keys(updates).filter((id) => {
        const u = updates[id];
        return u.phase === 'started' && nowSecs() - u.at < HEALTH_WATCH_SECS && live[id]?.health !== 'healthy';
      }),
    [updates, live],
  );
  const failedUpdates = useMemo(
    () => Object.entries(updates).filter(([, u]) => u.phase === 'failed'),
    [updates],
  );
  const phaseCounts = useMemo(() => {
    const counts: Record<string, number> = {};
    for (const id of active) counts[updates[id].phase] = (counts[updates[id].phase] ?? 0) + 1;
    return counts;
  }, [active, updates]);

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

  // A requested instance stops being "requested" once its record is here.
  // The timeout covers one the server skipped (already mid-update, or gone).
  useEffect(() => {
    setRequested((prev) => {
      const next = prev.filter((id) => !isUpdateActive(updates[id]));
      return next.length === prev.length ? prev : next;
    });
  }, [updates]);
  useEffect(() => {
    if (requested.length === 0) return;
    const id = setTimeout(() => setRequested([]), 15000);
    return () => clearTimeout(id);
  }, [requested]);

  // Once the last update in flight finishes, re-check digests once so the
  // rows go green on their own.
  const wasActive = useRef(false);
  useEffect(() => {
    const nowActive = active.length > 0;
    if (wasActive.current && !nowActive) void load();
    wasActive.current = nowActive;
  }, [active, load]);

  // The replicated record ends at "started"; whether a healthcheck still
  // has to pass is a live docker question. Polled only while some freshly
  // started instance has not reported healthy.
  const healthKey = healthWatch.join(',');
  useEffect(() => {
    if (!healthKey) return;
    const watched = healthKey.split(',');
    let cancelled = false;
    const tick = async () => {
      try {
        const instances = await api.listInstances();
        if (cancelled) return;
        setLive((prev) => {
          const next = { ...prev };
          for (const i of instances) if (watched.includes(i.id)) next[i.id] = i;
          return next;
        });
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
  }, [healthKey]);

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
   * Start updates. The server answers once every instance is queued in the
   * replicated update record, and rolls through them from there (one download and
   * one restart at a time per node); each step arrives here over the cluster state stream. The ids are
   * always sent explicitly — this view is looking at the answer already, and
   * letting the server recompute it would make it re-scan every registry
   * first.
   */
  async function startUpdate(ids: string[]) {
    setBusy(ids.length === 1 ? ids[0] : 'apply');
    setError(null);
    setResults(null);
    try {
      const { accepted } = await api.applyUpdates(ids);
      setRequested(accepted);
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
            disabled={busy !== null || inFlight.size > 0 || outdated.length === 0}
            onClick={() => void startUpdate(outdated.map((r) => r.id))}
          >
            {busy === 'apply' ? <span className="spinner" /> : <Icon name="download" />} Update all (
            {outdated.length})
          </button>
        </div>
      </div>

      {error && <p className="banner banner-bad">{error}</p>}

      {inFlight.size > 0 && (
        <p className="banner banner-warn">
          <span className="spinner" /> Updating {inFlight.size} instance(s), one download and one restart at a time per node
          {active.length > 0 &&
            ` (${Object.entries(phaseCounts)
              .map(([phase, n]) => `${n} ${UPDATE_PHASE_LABEL[phase as keyof typeof UPDATE_PHASE_LABEL]}`)
              .join(', ')})`}
          : each downloads its new image first, while the old container keeps serving traffic, then restarts onto
          it once the download lands. Watch progress per instance below — this keeps going whether or not you stay
          on this page.
        </p>
      )}

      {inFlight.size === 0 && failedUpdates.length > 0 && (
        <div className="banner banner-bad">
          {failedUpdates.length} update(s) failed:
          <ul>
            {failedUpdates.map(([id, u]) => (
              <li key={id}>
                <span className="mono">{id}</span> — {u.error ?? 'failed'}
              </li>
            ))}
          </ul>
        </div>
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
                    <UpdateCell
                      update={updates[row.id]}
                      requested={inFlight.has(row.id)}
                      health={live[row.id]?.health}
                      canUpdate={status === 'update'}
                      disabled={busy !== null || inFlight.size > 0}
                      onUpdate={() => void startUpdate([row.id])}
                    />
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

/** One row's update column: live progress while an update is in flight (or
 *  its new container is still passing its healthcheck), then how the last
 *  one ended next to the button for the next. */
function UpdateCell({
  update,
  requested,
  health,
  canUpdate,
  disabled,
  onUpdate,
}: {
  update: InstanceUpdate | undefined;
  requested: boolean;
  health: string | undefined;
  canUpdate: boolean;
  disabled: boolean;
  onUpdate: () => void;
}) {
  const settling = update?.phase === 'started' && health === 'starting';
  if (requested || settling) return <UpdateProgressBar update={update} health={health} />;
  return (
    <div className="btn-row">
      {update?.phase === 'started' && (
        <Badge tone="good" title={`new container started ${new Date(update.at * 1000).toLocaleTimeString()}`}>
          updated
        </Badge>
      )}
      {update?.phase === 'failed' && (
        <Badge tone="bad" title={update.error}>
          update failed
        </Badge>
      )}
      {canUpdate && (
        <button className="ghost" disabled={disabled} onClick={onUpdate}>
          <Icon name="download" /> Update
        </button>
      )}
    </div>
  );
}
