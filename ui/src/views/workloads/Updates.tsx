import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { ApiError, api } from '../../api/client';
import type { BulkResult, InstanceState, InstanceUpdate, RegistryAccount, UpdateRow } from '../../api/types';
import { Banner, Button, Chip, DataTable, EmptyState, Icon, PageHeader, Panel, StatusBadge, Tag, TextField, type Column } from '../../ds';
import { relativeTime, shortDigest } from '../../lib/format';
import { UPDATE_STEPS, isUpdateActive } from '../../lib/updates';
import { useClusterState } from '../../state/store';
import { RegistryAccounts } from './RegistryAccounts';
import './workloads.css';

type Status = 'update' | 'drift' | 'current' | 'unknown';
type UpdateStep = (typeof UPDATE_STEPS)[number];

/** One row's verdict, in the order that matters: an error means we do not
 *  know, and "we do not know" must never render as "up to date". */
function statusOf(row: UpdateRow): Status {
  if (row.error || !row.running_digest || !row.latest_digest) return 'unknown';
  if (row.update_available) return 'update';
  if (row.pin_drift) return 'drift';
  return 'current';
}

/** Rows needing action float to the top. */
const STATUS_PRIORITY: Record<Status, number> = { update: 0, drift: 1, unknown: 2, current: 3 };

const FILTERS: { status: Status; label: string }[] = [
  { status: 'update', label: 'Behind' },
  { status: 'drift', label: 'Off pin' },
  { status: 'unknown', label: 'Unknown' },
  { status: 'current', label: 'Up to date' },
];

/** How long after its new container started a row keeps watching its healthcheck. */
const HEALTH_WATCH_SECS = 180;
/** A finished update stays on its row for a day, then goes quiet. */
const RECENT_RESULT_SECS = 24 * 60 * 60;

/** Where dismissed failures are remembered: id -> the `at` of the failure that
 *  was dismissed. Failed records persist in replicated state, so without this
 *  the banner would never go away; matching on `at` means a newer failure of
 *  the same instance shows up again. */
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

const STEP_LABEL: Record<'queued' | UpdateStep, string> = {
  queued: 'Queued',
  pulling: 'Downloading',
  stopping: 'Stopping the old one',
  starting: 'Starting the new one',
  started: 'Started',
};

/** One row's live update progress, read from the replicated update record, so
 *  it is the same no matter which node this browser is talking to. */
function UpdateProgress({ phase, waitingForHealth }: { phase: 'queued' | UpdateStep; waitingForHealth: boolean }) {
  const step = phase === 'queued' ? 0 : UPDATE_STEPS.indexOf(phase) + 1;
  const label = waitingForHealth ? 'Waiting for the health check' : STEP_LABEL[phase];
  return (
    <div className="wl-progress" role="progressbar" aria-valuemin={0} aria-valuemax={UPDATE_STEPS.length} aria-valuenow={step} aria-valuetext={label}>
      <span className="steps" aria-hidden="true">
        {UPDATE_STEPS.map((p, i) => (
          <i key={p} className={i < step ? 'on' : undefined} />
        ))}
      </span>
      <span className="sub">
        {label} <span className="mono">{`${step}/${UPDATE_STEPS.length}`}</span>
      </span>
    </div>
  );
}

/** A digest that copies itself on click: the short form is for eyeballing, the
 *  full value is what you paste into a registry or a pin. */
function Digest({ digest, tone }: { digest: string; tone?: 'brand' | 'warn' | 'danger' }) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout>>();
  useEffect(() => () => clearTimeout(timer.current), []);
  const copy = () => {
    try {
      void navigator.clipboard?.writeText(digest)?.catch(() => {});
    } catch {
      // No clipboard (insecure context): nothing to copy to.
    }
    setCopied(true);
    clearTimeout(timer.current);
    timer.current = setTimeout(() => setCopied(false), 1200);
  };
  return (
    <button type="button" className={`wl-digest${tone ? ` wl-digest--${tone}` : ''}`} title={`Copy ${digest}`} onClick={copy}>
      {copied ? 'Copied' : shortDigest(digest)}
    </button>
  );
}

function VersionCell({ row, status }: { row: UpdateRow; status: Status }) {
  if (!row.running_digest) return <span>-</span>;
  return (
    <span className="wl-inline">
      {status === 'update' && row.latest_digest ? (
        <>
          <Digest digest={row.running_digest} />
          <span className="muted" aria-label="to">→</span>
          <Digest digest={row.latest_digest} tone="brand" />
        </>
      ) : status === 'drift' && row.pinned_digest ? (
        <>
          <Digest digest={row.running_digest} />
          <span className="muted" aria-label="should be">→</span>
          <Icon name="lock" size={14} />
          <Digest digest={row.pinned_digest} tone="danger" />
        </>
      ) : (
        <>
          <Digest digest={row.running_digest} />
          {row.pinned_digest && <Icon name="lock" size={14} label={`Pinned to ${row.pinned_digest}`} />}
        </>
      )}
    </span>
  );
}

/** The progress phase to draw for a row: its record's step while that is live,
 *  `started` while only the healthcheck is left, and `queued` otherwise. */
function stepOf(update: InstanceUpdate | undefined, waitingForHealth: boolean): 'queued' | UpdateStep {
  if (update && (update.phase === 'pulling' || update.phase === 'stopping' || update.phase === 'starting')) return update.phase;
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
   *  browser yet, so a row never flickers back to its button in between. The
   *  value is the `started_at` of the record that existed when the request was
   *  sent (0 for none). */
  const [requested, setRequested] = useState<Record<string, number>>({});
  /** Recently started instances' latest `/api/instances` poll, for the
   *  healthcheck that follows the last update step. */
  const [live, setLive] = useState<Record<string, InstanceState>>({});
  const [now, setNow] = useState(() => Date.now());
  const [dismissed, setDismissed] = useState<Record<string, number>>(loadDismissed);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [statusFilter, setStatusFilter] = useState<Status | null>(null);
  const [query, setQuery] = useState('');
  /** Every id accepted since nothing was last in flight, for "Updating 3 of 8". */
  const [batch, setBatch] = useState<Set<string>>(new Set());

  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 15000);
    return () => clearInterval(id);
  }, []);

  // Everything below is derived from the replicated record, so an update
  // started from another browser, the cli, or before this page was opened
  // shows up here all the same.
  const active = useMemo(() => Object.keys(updates).filter((id) => isUpdateActive(updates[id])), [updates]);
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
  const failedUpdates = useMemo(
    () => Object.entries(updates).filter(([id, u]) => u.phase === 'failed' && dismissed[id] !== u.at),
    [updates, dismissed],
  );

  const load = useCallback(async () => {
    setChecking(true);
    try {
      setRows(await api.listUpdates());
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
      // A node with no cluster key cannot open the logins. That is worth saying
      // inside the accounts sheet, not as a page-level error.
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

  // The batch grows while work is in flight and resets once the queue drains.
  useEffect(() => {
    setBatch((prev) => {
      if (inFlight.size === 0) return prev.size === 0 ? prev : new Set();
      if ([...inFlight].every((id) => prev.has(id))) return prev;
      return new Set([...prev, ...inFlight]);
    });
  }, [inFlight]);

  // Once the last update in flight finishes, re-check digests once so the
  // rows settle on their own.
  const wasActive = useRef(false);
  useEffect(() => {
    const nowActive = active.length > 0;
    if (wasActive.current && !nowActive) void load();
    wasActive.current = nowActive;
  }, [active, load]);

  // The replicated record ends at "started"; whether a healthcheck still has to
  // pass is a live docker question. Polled only while some freshly started
  // instance has not reported healthy.
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
  const sortedRows = useMemo(
    () =>
      [...(rows ?? [])].sort((a, b) => {
        const priority = STATUS_PRIORITY[statusOf(a)] - STATUS_PRIORITY[statusOf(b)];
        return priority !== 0 ? priority : a.id.localeCompare(b.id);
      }),
    [rows],
  );
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
  const behindIdle = useMemo(() => (rows ?? []).filter((r) => statusOf(r) === 'update' && !inFlight.has(r.id)), [rows, inFlight]);
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

  const allSelected = selectableIds.length > 0 && selectableIds.every((id) => selected.has(id));

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

  /** Start updates. The server answers once every instance is queued in the
   *  replicated update record and rolls through them from there (one download
   *  and one restart at a time per node). The ids are always sent explicitly:
   *  an empty list means "everything" server-side, and this view already has
   *  the answer. Instances already mid-update are skipped by the server, which
   *  is why queueing more while others run is safe. */
  async function startUpdate(ids: string[]) {
    if (ids.length === 0) return;
    setBusy(ids.length === 1 ? ids[0] : 'bulk');
    setError(null);
    setResults(null);
    try {
      const { accepted } = await api.applyUpdates(ids);
      setRequested((prev) => ({ ...prev, ...Object.fromEntries(accepted.map((id) => [id, updates[id]?.started_at ?? 0])) }));
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

  const toggleSelected = (id: string) =>
    setSelected((prev) => {
      const next = new Set(prev);
      if (!next.delete(id)) next.add(id);
      return next;
    });

  if (!rows) {
    return error ? (
      <Banner tone="danger" title="Could not check for updates">
        {error}
      </Banner>
    ) : (
      <p className="sub" role="status">
        Checking registries...
      </p>
    );
  }

  const failures = (results ?? []).filter((r) => !r.ok);
  const retryable = failedUpdates
    .map(([id]) => id)
    .filter((id) => rows.some((r) => r.id === id && statusOf(r) === 'update') && !inFlight.has(id));
  const checkedLabel = checkedAt ? `Checked ${relativeTime(checkedAt, now)}` : 'Not checked yet';
  const subtitle =
    rows.length === 0
      ? `No containers · ${checkedLabel}`
      : counts.current === rows.length
        ? `All ${rows.length} ${rows.length === 1 ? 'container is' : 'containers are'} on the newest image · ${checkedLabel}`
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
  const batchDone = [...batchAll].filter((id) => !inFlight.has(id) && (updates[id]?.phase === 'started' || updates[id]?.phase === 'failed')).length;
  const filtering = statusFilter !== null || query.trim() !== '';

  const columns: Column<UpdateRow>[] = [
    {
      key: 'select',
      label: 'Select',
      render: (_, row) => (
        <input
          type="checkbox"
          className="wl-check"
          aria-label={`Select ${row.id}`}
          checked={selected.has(row.id)}
          disabled={statusOf(row) !== 'update' || rowInFlight(row.id)}
          onChange={() => toggleSelected(row.id)}
        />
      ),
    },
    {
      key: 'id',
      label: 'Container',
      render: (_, row) => (
        <span className="wl-stack">
          <Link className="lnk" to={`/instances/${encodeURIComponent(row.id)}`}>
            {row.id}
          </Link>
          <span className="code-sm muted" title={row.image ?? undefined}>
            {row.image ?? '-'}
          </span>
        </span>
      ),
    },
    { key: 'version', label: 'Image', render: (_, row) => <VersionCell row={row} status={statusOf(row)} /> },
    {
      key: 'status',
      label: 'Status',
      render: (_, row) => {
        const status = statusOf(row);
        return (
          <span className="wl-stack">
            {status === 'update' ? (
              <Tag tone="brand">Update available</Tag>
            ) : status === 'drift' ? (
              <StatusBadge status="degraded" label="Not on its pin" />
            ) : status === 'unknown' ? (
              <StatusBadge status="stopped" label="Unknown" />
            ) : (
              <StatusBadge status="healthy" label="Up to date" />
            )}
            {status === 'unknown' && row.error && (
              <span className="sub" title={row.error}>
                {row.error}{' '}
                {AUTH_ERROR.test(row.error) && (
                  <button type="button" className="wl-link" onClick={() => setAccountsOpen(true)}>
                    Add registry login
                  </button>
                )}
              </span>
            )}
            {status === 'drift' && (
              <Link className="lnk sub" to={`/instances/${encodeURIComponent(row.id)}`}>
                Redeploy to apply the pin
              </Link>
            )}
          </span>
        );
      },
    },
    { key: 'node', label: 'Node' },
    {
      key: 'actions',
      label: 'Action',
      render: (_, row) => {
        const status = statusOf(row);
        const update = updates[row.id];
        const flight = rowInFlight(row.id);
        const failedVisible = update?.phase === 'failed' && dismissed[row.id] !== update.at;
        const recent = update?.phase === 'started' && nowSecs - update.at < RECENT_RESULT_SECS;
        const waitingForHealth = update?.phase === 'started' && live[row.id]?.health === 'starting' && !inFlight.has(row.id);
        if (flight) return <UpdateProgress phase={stepOf(update, !!waitingForHealth)} waitingForHealth={!!waitingForHealth} />;
        return (
          <span className="wl-stack">
            {recent && update && <Tag tone="ok">{`Updated ${relativeTime(update.at * 1000, now)}`}</Tag>}
            {failedVisible && (
              <span className="wl-danger" title={update.error}>
                <Icon name="x" size={14} /> Failed
              </span>
            )}
            {status === 'update' && (
              <Button size="sm" icon={failedVisible ? 'refresh' : 'download'} disabled={busy === row.id || busy === 'bulk'} onClick={() => void startUpdate([row.id])}>
                {failedVisible ? `Retry ${row.id}` : `Update ${row.id}`}
              </Button>
            )}
          </span>
        );
      },
    },
  ];

  return (
    <div className="sec wl-page">
      <PageHeader
        eyebrow="Cluster"
        title="Updates"
        sub={subtitle}
        actions={
          <>
            <Button icon="key" onClick={() => setAccountsOpen(true)}>
              {`Registry accounts (${accounts.length})`}
            </Button>
            <Button icon="lock" disabled={busy !== null || unpinned.length === 0} title="Record what each unpinned container runs as its pin, so it cannot drift on a later restart" onClick={() => void runPin()}>
              {`Pin running digests (${unpinned.length})`}
            </Button>
            <Button icon="refresh" loading={checking} onClick={() => void load()}>
              {checking ? 'Checking' : 'Check again'}
            </Button>
            {selected.size > 0 ? (
              <Button variant="primary" icon="download" loading={busy === 'bulk'} disabled={busy !== null} onClick={() => void startUpdate([...selected])}>
                {`Update ${selected.size} ${selected.size === 1 ? 'container' : 'containers'}`}
              </Button>
            ) : (
              <Button variant="primary" icon="download" disabled={busy !== null || behindIdle.length === 0} loading={busy === 'bulk'} onClick={() => void startUpdate(behindIdle.map((r) => r.id))}>
                {`Update all behind (${behindIdle.length})`}
              </Button>
            )}
          </>
        }
      />

      {error && (
        <Banner tone="danger" title="Something went wrong" onClose={() => setError(null)}>
          {error}
        </Banner>
      )}

      {failedUpdates.length > 0 && (
        <Banner
          tone="danger"
          title={`${failedUpdates.length} ${failedUpdates.length === 1 ? 'update' : 'updates'} failed`}
          action={retryable.length > 0 ? 'Retry failed' : undefined}
          onAction={() => void startUpdate(retryable)}
          onClose={dismissFailures}
        >
          {failedUpdates
            .slice(0, 5)
            .map(([id, u]) => `${id}: ${u.error ?? 'failed'}`)
            .join('. ')}
          {failedUpdates.length > 5 ? `. And ${failedUpdates.length - 5} more.` : ''}
        </Banner>
      )}

      {results && (
        <Banner
          tone={failures.length === 0 ? 'ok' : 'warn'}
          title={results.length === 0 ? 'Nothing to pin' : `${results.length - failures.length} of ${results.length} succeeded`}
          onClose={() => setResults(null)}
        >
          {failures.map((f) => `${f.id}: ${f.error ?? 'failed'}`).join('. ')}
        </Banner>
      )}

      {inFlight.size > 0 && (
        <Panel title={`Updating ${batchDone} of ${batchAll.size}`}>
          <div className="wl-bar" role="progressbar" aria-label="Overall update progress" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(fraction * 100)}>
            <span style={{ width: `${fraction * 100}%` }} />
          </div>
          <p className="sub">
            {[
              stripPhases.downloading > 0 && `${stripPhases.downloading} downloading`,
              stripPhases.restarting > 0 && `${stripPhases.restarting} restarting`,
              stripPhases.queued > 0 && `${stripPhases.queued} queued`,
            ]
              .filter(Boolean)
              .join(' · ')}
            . Old containers keep serving until their new image is downloaded, one download and one restart at a time per node. It is safe to leave this page.
          </p>
        </Panel>
      )}

      <div className="chips" role="group" aria-label="Filter by status">
        {FILTERS.map((f) => (
          <Chip key={f.status} label={f.label} count={counts[f.status]} selected={statusFilter === f.status} onClick={() => setStatusFilter(statusFilter === f.status ? null : f.status)} />
        ))}
        {selectableIds.length > 0 && (
          <Chip label={allSelected ? 'Clear selection' : 'Select all behind'} icon="check" onClick={() => setSelected(allSelected ? new Set() : new Set(selectableIds))} />
        )}
      </div>
      <TextField label="Filter by container, image or node" type="search" value={query} onChange={(e) => setQuery(e.target.value)} />

      {visibleRows.length === 0 ? (
        <EmptyState
          icon="download"
          title={rows.length === 0 ? 'Nothing is running yet' : 'No containers match'}
          action={
            filtering ? (
              <Button
                onClick={() => {
                  setStatusFilter(null);
                  setQuery('');
                }}
              >
                Clear filters
              </Button>
            ) : undefined
          }
        >
          {rows.length === 0 ? 'Deploy a stack and its images are checked here.' : 'Try another filter.'}
        </EmptyState>
      ) : (
        <DataTable caption="Image updates" columns={columns} rows={visibleRows} />
      )}

      <RegistryAccounts open={accountsOpen} accounts={accounts} onClose={() => setAccountsOpen(false)} onChanged={() => void loadAccounts()} />
    </div>
  );
}
