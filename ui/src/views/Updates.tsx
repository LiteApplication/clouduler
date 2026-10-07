import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { ApiError, api } from '../api/client';
import type { BulkResult, InstanceState, InstanceUpdate, RegistryAccount, UpdateRow } from '../api/types';
import { Badge } from '../components/Badge';
import { Icon } from '../components/Icon';
import { UPDATE_STEPS, isUpdateActive } from '../lib/updates';
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


/** How long after its new container started a row keeps watching its
 *  healthcheck, via the `/api/instances` poll. */
const HEALTH_WATCH_SECS = 180;

/** A finished update stays on its row as "updated · 4 min ago" for a day,
 *  then goes quiet: a green badge forever says nothing about today. */
const RECENT_RESULT_SECS = 24 * 60 * 60;

/** Where dismissed failures are remembered: id -> the `at` of the failure
 *  that was dismissed. Failed records persist in replicated state, so without
 *  this the banner would never go away; matching on `at` means a newer failure
 *  of the same instance shows up again. */
const DISMISSED_KEY = 'clouduler.updates.dismissedFailures';

/** Looks like a registry login problem, as opposed to an outage or a typo. */
const AUTH_ERROR = /401|403|unauthori[sz]ed|denied|auth/i;

function loadDismissed(): Record<string, number> {
  try {
    const raw = localStorage.getItem(DISMISSED_KEY);
    const parsed: unknown = raw ? JSON.parse(raw) : {};
    return parsed && typeof parsed === 'object' ? (parsed as Record<string, number>) : {};
  } catch {
    return {};
  }
}

function saveDismissed(map: Record<string, number>) {
  try {
    localStorage.setItem(DISMISSED_KEY, JSON.stringify(map));
  } catch {
    // Storage blocked (private window, quota): the dismissal just lasts until reload.
  }
}

function relativeTime(thenMs: number, nowMs: number): string {
  const secs = Math.max(0, Math.floor((nowMs - thenMs) / 1000));
  if (secs < 60) return 'just now';
  if (secs < 3600) return `${Math.floor(secs / 60)} min ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)} h ago`;
  return `${Math.floor(secs / 86400)} d ago`;
}

/** The step label shown on a row while it is being updated. */
const STEP_LABEL: Record<'queued' | UpdateStep, string> = {
  queued: 'queued',
  pulling: 'downloading',
  stopping: 'stopping old',
  starting: 'starting new',
  started: 'started',
};

type UpdateStep = (typeof UPDATE_STEPS)[number];

/**
 * One row's live update progress, read from the replicated update record
 * (`ClusterState.instance_updates`), which the node doing the work writes
 * at every real step — so it is the same no matter which node this browser
 * is talking to. `phase` is `queued` in the instant between the server
 * accepting the request and its record reaching this browser, and for a
 * retried row whose old `failed` record has not been replaced yet: a failed
 * record is never progress, so callers pass `queued` rather than `failed`.
 * `waitingForHealth` comes from the `/api/instances` poll: once the new
 * container is started, its healthcheck (if it has one) still has to pass.
 */
function UpdateProgress({ phase, waitingForHealth }: { phase: 'queued' | UpdateStep; waitingForHealth: boolean }) {
  const step = phase === 'queued' ? 0 : UPDATE_STEPS.indexOf(phase) + 1;
  const label = waitingForHealth ? 'waiting for health check' : STEP_LABEL[phase];
  return (
    <div
      className="update-progress"
      role="progressbar"
      aria-valuemin={0}
      aria-valuemax={UPDATE_STEPS.length}
      aria-valuenow={step}
      aria-valuetext={label}
    >
      <div className="update-progress-head">
        <span className="spinner" />
        <span>{label}</span>
        <span className="mono faint update-progress-count">
          {step}/{UPDATE_STEPS.length}
        </span>
      </div>
      <div className="update-progress-track" aria-hidden="true">
        {UPDATE_STEPS.map((p, i) => (
          <span
            key={p}
            className={`update-progress-seg${i < step - 1 ? ' done' : ''}${i === step - 1 ? ' active' : ''}`}
          />
        ))}
      </div>
    </div>
  );
}

/** A digest that copies itself on click: the short form is for eyeballing,
 *  the full value is what you paste into a registry or a pin. */
function Digest({ digest, className }: { digest: string; className?: string }) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout>>();
  useEffect(() => () => clearTimeout(timer.current), []);
  function copy() {
    try {
      void navigator.clipboard?.writeText(digest)?.catch(() => {});
    } catch {
      // No clipboard (insecure context): nothing to copy to.
    }
    setCopied(true);
    clearTimeout(timer.current);
    timer.current = setTimeout(() => setCopied(false), 1200);
  }
  return (
    <button type="button" className={`digest${className ? ` ${className}` : ''}`} title={digest} onClick={copy}>
      {copied ? (
        <>
          <Icon name="check" /> copied
        </>
      ) : (
        shortDigest(digest)
      )}
    </button>
  );
}

function VersionCell({ row, status }: { row: UpdateRow; status: Status }) {
  const running = row.running_digest ? <Digest digest={row.running_digest} /> : <span>-</span>;
  return (
    <span className="version-cell">
      {status === 'update' && row.running_digest && row.latest_digest ? (
        <>
          <Digest digest={row.running_digest} className="dim" />
          <span className="faint">→</span>
          <Digest digest={row.latest_digest} className="warn" />
        </>
      ) : status === 'drift' && row.running_digest && row.pinned_digest ? (
        <>
          <Digest digest={row.running_digest} />
          <span className="faint">→</span>
          <Icon name="lock" />
          <Digest digest={row.pinned_digest} className="bad" />
        </>
      ) : status === 'current' ? (
        <>
          {running}
          <span className="version-ok" title="up to date">
            <Icon name="check" />
          </span>
        </>
      ) : (
        running
      )}
      {row.pinned_digest && status !== 'drift' && (
        <span className="version-pin" title={`pinned to ${row.pinned_digest}`}>
          <Icon name="lock" />
        </span>
      )}
    </span>
  );
}

const TILES: { status: Status; label: string; icon: 'download' | 'alert' | 'alert-circle' | 'check-circle'; tone: string }[] = [
  { status: 'update', label: 'Behind', icon: 'download', tone: 'warn' },
  { status: 'drift', label: 'Off pin', icon: 'alert', tone: 'bad' },
  { status: 'unknown', label: 'Unknown', icon: 'alert-circle', tone: 'neutral' },
  { status: 'current', label: 'Up to date', icon: 'check-circle', tone: 'good' },
];

/** The progress phase to draw for a row: its record's step while that is
 *  live, `started` while only the healthcheck is left, and `queued` for
 *  everything else — never `failed`, which is a result, not progress. */
function stepOf(update: InstanceUpdate | undefined, waitingForHealth: boolean): 'queued' | UpdateStep {
  if (update && (update.phase === 'pulling' || update.phase === 'stopping' || update.phase === 'starting')) {
    return update.phase;
  }
  return waitingForHealth ? 'started' : 'queued';
}

export function Updates() {
  const [rows, setRows] = useState<UpdateRow[] | null>(null);
  const [accounts, setAccounts] = useState<RegistryAccount[]>([]);
  const [accountsOpen, setAccountsOpen] = useState(false);
  /** Only the in-progress HTTP request: a row id, 'bulk' or 'pin'. */
  const [busy, setBusy] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);
  const [checkedAt, setCheckedAt] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [results, setResults] = useState<BulkResult[] | null>(null);
  const cluster = useClusterState();
  const updates = useMemo(() => cluster?.instance_updates ?? {}, [cluster]);
  /** Instances an `apply` accepted whose new record has not reached this
   *  browser yet, so a row never flickers back to its button in between.
   *  The value is the `started_at` of the record that existed when the
   *  request was sent (0 for none): a record with a different `started_at`
   *  is the new update's, even if it already ended (a retry that failed
   *  before this browser saw an active phase would otherwise sit on a fake
   *  "queued" until the timeout). */
  const [requested, setRequested] = useState<Record<string, number>>({});
  /** Recently started instances' latest `/api/instances` poll, for the
   *  healthcheck that follows the last update step. */
  const [live, setLive] = useState<Record<string, InstanceState>>({});
  /** Wall clock, ticking so relative times and the health-watch window move
   *  on instead of freezing at whatever the last render saw. */
  const [now, setNow] = useState(() => Date.now());
  const [dismissed, setDismissed] = useState<Record<string, number>>(loadDismissed);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [statusFilter, setStatusFilter] = useState<Status | null>(null);
  const [query, setQuery] = useState('');
  /** Every id accepted since nothing was last in flight, for "Updating 3/8". */
  const [batch, setBatch] = useState<Set<string>>(new Set());

  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 15000);
    return () => clearInterval(id);
  }, []);

  // Everything below is derived from the replicated record, so an update
  // started from another browser, the cli, or before this page was opened
  // shows up here all the same.
  const active = useMemo(
    () => Object.keys(updates).filter((id) => isUpdateActive(updates[id])),
    [updates],
  );
  const inFlight = useMemo(() => new Set([...active, ...Object.keys(requested)]), [active, requested]);
  const nowSecs = Math.floor(now / 1000);
  const healthWatch = useMemo(
    () =>
      Object.keys(updates).filter((id) => {
        const u = updates[id];
        return u.phase === 'started' && nowSecs - u.at < HEALTH_WATCH_SECS && live[id]?.health !== 'healthy';
      }),
    [updates, live, nowSecs],
  );
  /** Failures the user has not dismissed. */
  const failedUpdates = useMemo(
    () => Object.entries(updates).filter(([id, u]) => u.phase === 'failed' && dismissed[id] !== u.at),
    [updates, dismissed],
  );

  const load = useCallback(async () => {
    setChecking(true);
    try {
      const list = await api.listUpdates();
      setRows(list);
      setCheckedAt(Date.now());
      setError(null);
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e));
    } finally {
      setChecking(false);
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

  // A requested instance stops being "requested" once its new record is here.
  // The timeout covers one the server skipped (already mid-update, or gone).
  useEffect(() => {
    setRequested((prev) => {
      const ids = Object.keys(prev);
      const keep = ids.filter((id) => {
        const u = updates[id];
        return !u || ((u.phase === 'started' || u.phase === 'failed') && u.started_at === prev[id]);
      });
      if (keep.length === ids.length) return prev;
      return Object.fromEntries(keep.map((id) => [id, prev[id]]));
    });
  }, [updates]);
  useEffect(() => {
    if (Object.keys(requested).length === 0) return;
    const id = setTimeout(() => setRequested({}), 15000);
    return () => clearTimeout(id);
  }, [requested]);

  // Track the batch: it grows while work is in flight and resets once the
  // queue drains, so the next round starts counting from zero.
  useEffect(() => {
    setBatch((prev) => {
      if (inFlight.size === 0) return prev.size === 0 ? prev : new Set();
      if ([...inFlight].every((id) => prev.has(id))) return prev;
      return new Set([...prev, ...inFlight]);
    });
  }, [inFlight]);

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

  // A pin result that fully succeeded has nothing left to say.
  useEffect(() => {
    if (!results || results.some((r) => !r.ok)) return;
    const id = setTimeout(() => setResults(null), 8000);
    return () => clearTimeout(id);
  }, [results]);

  const counts = useMemo(() => {
    const c: Record<Status, number> = { update: 0, drift: 0, unknown: 0, current: 0 };
    for (const r of rows ?? []) c[statusOf(r)] += 1;
    return c;
  }, [rows]);
  const unpinned = useMemo(() => (rows ?? []).filter((r) => !r.pinned_digest && r.running_digest), [rows]);
  // Needs-an-update rows first, so the table opens on what there is to act
  // on rather than making it scroll past everything already current.
  const sortedRows = useMemo(() => {
    return [...(rows ?? [])].sort((a, b) => {
      const priority = STATUS_PRIORITY[statusOf(a)] - STATUS_PRIORITY[statusOf(b)];
      return priority !== 0 ? priority : a.id.localeCompare(b.id);
    });
  }, [rows]);
  const visibleRows = useMemo(() => {
    const q = query.trim().toLowerCase();
    return sortedRows.filter((r) => {
      if (statusFilter && statusOf(r) !== statusFilter) return false;
      if (!q) return true;
      return [r.id, r.image, r.node, r.stack, r.service].some((f) => f?.toLowerCase().includes(q));
    });
  }, [sortedRows, statusFilter, query]);

  /** Whether this row's update is running (or just requested, or its new
   *  container is still passing its healthcheck). */
  const rowInFlight = useCallback(
    (id: string) => inFlight.has(id) || (updates[id]?.phase === 'started' && live[id]?.health === 'starting'),
    [inFlight, updates, live],
  );
  const behindIdle = useMemo(
    () => (rows ?? []).filter((r) => statusOf(r) === 'update' && !inFlight.has(r.id)),
    [rows, inFlight],
  );
  const selectableIds = useMemo(
    () => visibleRows.filter((r) => statusOf(r) === 'update' && !rowInFlight(r.id)).map((r) => r.id),
    [visibleRows, rowInFlight],
  );

  // Selection only ever holds rows the user can still act on.
  useEffect(() => {
    setSelected((prev) => {
      const ok = new Set(selectableIds);
      const next = new Set([...prev].filter((id) => ok.has(id)));
      return next.size === prev.size ? prev : next;
    });
  }, [selectableIds]);

  const headCheck = useRef<HTMLInputElement>(null);
  const allSelected = selectableIds.length > 0 && selectableIds.every((id) => selected.has(id));
  const someSelected = selected.size > 0 && !allSelected;
  useEffect(() => {
    if (headCheck.current) headCheck.current.indeterminate = someSelected;
  }, [someSelected]);

  async function runPin() {
    setBusy('pin');
    setError(null);
    setResults(null);
    try {
      setResults(await api.pinUpdates());
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
   * always sent explicitly — an empty list means "everything" server-side, and this view is
   * looking at the answer already; letting the server recompute it would make it re-scan every
   * registry first. Instances already mid-update are skipped by the server, which is why
   * queueing more while others run is safe.
   */
  async function startUpdate(ids: string[]) {
    if (ids.length === 0) return;
    setBusy(ids.length === 1 ? ids[0] : 'bulk');
    setError(null);
    setResults(null);
    try {
      const { accepted } = await api.applyUpdates(ids);
      setRequested((prev) => ({
        ...prev,
        ...Object.fromEntries(accepted.map((id) => [id, updates[id]?.started_at ?? 0])),
      }));
      setSelected(new Set());
    } catch (e) {
      setError(e instanceof ApiError ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  }

  function dismissFailures() {
    const next = { ...dismissed };
    for (const [id, u] of Object.entries(updates)) if (u.phase === 'failed') next[id] = u.at;
    setDismissed(next);
    saveDismissed(next);
  }

  function toggleSelected(id: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (!next.delete(id)) next.add(id);
      return next;
    });
  }

  function clearFilters() {
    setStatusFilter(null);
    setQuery('');
  }

  if (!rows) {
    return (
      <p className="loading">
        <span className="spinner" /> Checking registries...
      </p>
    );
  }

  const failures = (results ?? []).filter((r) => !r.ok);
  const retryable = failedUpdates
    .map(([id]) => id)
    .filter((id) => rows.some((r) => r.id === id && statusOf(r) === 'update') && !inFlight.has(id));
  const checkedLabel = checkedAt ? `checked ${relativeTime(checkedAt, now)}` : 'not checked yet';
  const subtitle =
    rows.length === 0
      ? `no containers · ${checkedLabel}`
      : counts.current === rows.length
        ? `All ${rows.length} container${rows.length === 1 ? '' : 's'} on their newest image · ${checkedLabel}`
        : [
            counts.update > 0 && `${counts.update} behind`,
            counts.drift > 0 && `${counts.drift} off their pin`,
            counts.unknown > 0 && `${counts.unknown} unknown`,
            checkedLabel,
          ]
            .filter(Boolean)
            .join(' · ');

  // In-flight summary for the progress strip.
  const stripPhases = { downloading: 0, restarting: 0, queued: 0 };
  let fraction = 0;
  for (const id of inFlight) {
    const u = updates[id];
    const phase = isUpdateActive(u) ? u.phase : 'queued';
    if (phase === 'pulling') stripPhases.downloading += 1;
    else if (phase === 'stopping' || phase === 'starting') stripPhases.restarting += 1;
    else stripPhases.queued += 1;
    if (phase !== 'queued' && phase !== 'failed') fraction += (UPDATE_STEPS.indexOf(phase) + 1) / UPDATE_STEPS.length;
  }
  fraction = inFlight.size > 0 ? fraction / inFlight.size : 0;
  const batchAll = new Set([...batch, ...inFlight]);
  const batchDone = [...batchAll].filter(
    (id) => !inFlight.has(id) && (updates[id]?.phase === 'started' || updates[id]?.phase === 'failed'),
  ).length;

  const activeTile = TILES.find((t) => t.status === statusFilter);
  const filtering = statusFilter !== null || query.trim() !== '';

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="download" />
          <div>
            <h1>Updates</h1>
            <p className="page-sub">{subtitle}</p>
          </div>
        </div>
        <div className="btn-row">
          <button className="ghost" onClick={() => setAccountsOpen(true)}>
            <Icon name="key" /> Registry accounts ({accounts.length})
          </button>
          <button
            className="ghost"
            disabled={busy !== null || unpinned.length === 0}
            title="Record what each unpinned container is running as its pin, so it cannot drift on a later restart"
            onClick={() => void runPin()}
          >
            <Icon name="lock" /> Pin running ({unpinned.length})
          </button>
          <button className="ghost" onClick={() => void load()} disabled={checking}>
            {checking ? <span className="spinner" /> : <Icon name="refresh" />} {checking ? 'Checking…' : 'Re-check'}
          </button>
        </div>
      </div>

      {error && <p className="banner banner-bad">{error}</p>}

      <div className="card-grid updates-tiles">
        {TILES.map((t) => (
          <button
            key={t.status}
            type="button"
            className={`card stat stat-tile tone-${t.tone}`}
            aria-pressed={statusFilter === t.status}
            onClick={() => setStatusFilter(statusFilter === t.status ? null : t.status)}
          >
            <span className="stat-head">
              <Icon name={t.icon} />
              <span className="stat-label">{t.label}</span>
            </span>
            <span className={`stat-value${counts[t.status] === 0 ? ' zero' : ''}`}>{counts[t.status]}</span>
          </button>
        ))}
      </div>

      {inFlight.size > 0 && (
        <div className="card updates-progress-strip">
          <div className="updates-strip-line">
            <span className="spinner" />
            <strong>
              Updating {batchDone}/{batchAll.size}
            </strong>
            <span className="muted">
              {[
                stripPhases.downloading > 0 && `${stripPhases.downloading} downloading`,
                stripPhases.restarting > 0 && `${stripPhases.restarting} restarting`,
                stripPhases.queued > 0 && `${stripPhases.queued} queued`,
              ]
                .filter(Boolean)
                .join(' · ')}
            </span>
          </div>
          <div
            className="updates-bar"
            role="progressbar"
            aria-label="Overall update progress"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={Math.round(fraction * 100)}
          >
            <span style={{ width: `${fraction * 100}%` }} />
          </div>
          <p className="faint updates-strip-note">
            Old containers keep serving until their new image is downloaded. One download and one restart at a time per
            node — safe to leave this page.
          </p>
        </div>
      )}

      {failedUpdates.length > 0 && (
        <div className="banner banner-bad updates-failed">
          <Icon name="alert" />
          <div className="updates-failed-body">
            <div>
              {failedUpdates.length} update{failedUpdates.length === 1 ? '' : 's'} failed
            </div>
            <ul>
              {failedUpdates.slice(0, 5).map(([id, u]) => (
                <li key={id}>
                  <span className="mono">{id}</span> — {u.error ?? 'failed'}
                </li>
              ))}
              {failedUpdates.length > 5 && <li>+{failedUpdates.length - 5} more</li>}
            </ul>
          </div>
          {retryable.length > 0 && (
            <button className="ghost" disabled={busy !== null} onClick={() => void startUpdate(retryable)}>
              <Icon name="refresh" /> Retry
            </button>
          )}
          <button className="ghost icon-btn" aria-label="Dismiss failures" onClick={dismissFailures}>
            <Icon name="close" />
          </button>
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

      <div className="updates-toolbar">
        <div className="updates-toolbar-filters">
          <div className="flex-row updates-search">
            <Icon name="search" style={{ color: 'var(--text-faint)' }} />
            <input
              type="search"
              placeholder="filter by instance, image, node"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              aria-label="Filter updates"
            />
          </div>
          {activeTile && (
            <span className={`badge badge-${STATUS_TONE[activeTile.status]}`}>
              {activeTile.label}
              <button
                type="button"
                className="chip-x"
                aria-label={`Clear ${activeTile.label} filter`}
                onClick={() => setStatusFilter(null)}
              >
                <Icon name="close" />
              </button>
            </span>
          )}
        </div>
        <div className="updates-actionbar">
          {selected.size > 0 ? (
            <>
              <span className="muted">{selected.size} selected</span>
              <button className="ghost" onClick={() => setSelected(new Set())}>
                Clear
              </button>
              <button className="primary" disabled={busy !== null} onClick={() => void startUpdate([...selected])}>
                {busy === 'bulk' ? <span className="spinner" /> : <Icon name="download" />} Update {selected.size}
              </button>
            </>
          ) : (
            <button
              className="primary"
              disabled={busy !== null || behindIdle.length === 0}
              onClick={() => void startUpdate(behindIdle.map((r) => r.id))}
            >
              {busy === 'bulk' ? <span className="spinner" /> : <Icon name="download" />} Update all behind (
              {behindIdle.length})
            </button>
          )}
        </div>
      </div>

      <div className="table-wrap updates-wrap">
        <table className="updates-table">
          <thead>
            <tr>
              <th className="col-check">
                <input
                  ref={headCheck}
                  type="checkbox"
                  aria-label="Select all updatable"
                  checked={allSelected}
                  disabled={selectableIds.length === 0}
                  onChange={() => setSelected(allSelected ? new Set() : new Set(selectableIds))}
                />
              </th>
              <th>Instance</th>
              <th>Version</th>
              <th>Status</th>
              <th>Node</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {visibleRows.map((row) => {
              const status = statusOf(row);
              const update = updates[row.id];
              const flight = rowInFlight(row.id);
              const selectable = status === 'update' && !flight;
              const failedVisible = update?.phase === 'failed' && dismissed[row.id] !== update.at;
              const recent = update?.phase === 'started' && nowSecs - update.at < RECENT_RESULT_SECS;
              const waitingForHealth = update?.phase === 'started' && live[row.id]?.health === 'starting' && !inFlight.has(row.id);
              const sending = busy === row.id || busy === 'bulk';
              return (
                <tr
                  key={row.id}
                  className={`${flight ? 'is-flight' : ''}${selected.has(row.id) ? ' is-selected' : ''}`}
                >
                  <td className="col-check">
                    <input
                      type="checkbox"
                      aria-label={`Select ${row.id}`}
                      checked={selected.has(row.id)}
                      disabled={!selectable}
                      onChange={() => toggleSelected(row.id)}
                    />
                  </td>
                  <td className="col-instance">
                    <Link className="row-link" to={`/instances/${encodeURIComponent(row.id)}`}>
                      {row.id}
                    </Link>
                    <span className="updates-image mono faint" title={row.image ?? undefined}>
                      {row.image ?? '-'}
                    </span>
                  </td>
                  <td className="col-version mono">
                    <VersionCell row={row} status={status} />
                  </td>
                  <td className="col-status">
                    <Badge tone={STATUS_TONE[status]}>{STATUS_LABEL[status]}</Badge>
                    {status === 'unknown' && row.error && (
                      <div className="updates-note faint" title={row.error}>
                        <span className="updates-error">{row.error}</span>
                        {AUTH_ERROR.test(row.error) && (
                          <button type="button" className="link-btn" onClick={() => setAccountsOpen(true)}>
                            Add registry login
                          </button>
                        )}
                      </div>
                    )}
                    {status === 'drift' && (
                      <div className="updates-note faint">
                        <Link to={`/instances/${encodeURIComponent(row.id)}`}>redeploy to apply the pin</Link>
                      </div>
                    )}
                  </td>
                  <td className="col-node muted">{row.node}</td>
                  <td className="col-actions">
                    <div className="updates-actions">
                      {flight ? (
                        <UpdateProgress
                          phase={stepOf(update, waitingForHealth)}
                          waitingForHealth={waitingForHealth}
                        />
                      ) : (
                        <>
                          {recent && update && (
                            <Badge
                              tone="good"
                              title={`new container started ${new Date(update.at * 1000).toLocaleTimeString()}`}
                            >
                              updated · {relativeTime(update.at * 1000, now)}
                            </Badge>
                          )}
                          {failedVisible && (
                            <Badge tone="bad" title={update.error}>
                              failed
                            </Badge>
                          )}
                          {status === 'update' && (
                            <button className="ghost" disabled={sending} onClick={() => void startUpdate([row.id])}>
                              {failedVisible ? <Icon name="refresh" /> : <Icon name="download" />}{' '}
                              {failedVisible ? 'Retry' : 'Update'}
                            </button>
                          )}
                        </>
                      )}
                    </div>
                  </td>
                </tr>
              );
            })}
            {visibleRows.length === 0 && (
              <tr className="empty-row">
                <td colSpan={6} className="empty-cell">
                  {rows.length === 0 ? (
                    'nothing is running yet'
                  ) : (
                    <>
                      no containers match
                      {filtering && (
                        <>
                          {' '}
                          <button className="ghost" onClick={clearFilters}>
                            Clear filters
                          </button>
                        </>
                      )}
                    </>
                  )}
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
