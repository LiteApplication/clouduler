import { useEffect, useState } from 'react';
import { useParams } from 'react-router-dom';
import { api, ApiError } from '../api/client';
import type { InstanceState, InstanceStatsSample, LogLine } from '../api/types';
import { useClusterState } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { Badge } from '../components/Badge';
import { useEventSource } from '../hooks/useEventSource';
import { formatBytes } from '../lib/format';
import { Icon } from '../components/Icon';

export function InstanceDetail() {
  const { id = '' } = useParams();
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const [instance, setInstance] = useState<InstanceState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [moveTarget, setMoveTarget] = useState('');
  const [tab, setTab] = useState<'logs' | 'stats'>('logs');

  useEffect(() => {
    let cancelled = false;
    const load = () =>
      api
        .getInstance(id)
        .then((i) => !cancelled && setInstance(i))
        .catch((e) => !cancelled && setError(e instanceof ApiError ? e.message : String(e)));
    load();
    const timer = setInterval(load, 4000);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [id]);

  const run = async (action: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await action();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'action failed');
    } finally {
      setBusy(false);
    }
  };

  if (!instance) {
    return error ? (
      <p className="field-error">
        <Icon name="alert-circle" /> {error}
      </p>
    ) : (
      <p className="loading">
        <span className="spinner" /> Loading instance...
      </p>
    );
  }

  const nodes = cluster ? Object.values(cluster.nodes) : [];
  const eligible = nodes.filter(
    (n) => n.name !== instance.node && n.availability === 'active' && !cluster?.unreachable.includes(n.name),
  );
  const isLatest = (instance.image ?? '').endsWith(':latest');

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="instances" />
          <div>
            <h1>{instance.id}</h1>
            <p className="page-sub">on {instance.node}</p>
          </div>
        </div>
        <Badge tone={instance.state === 'running' ? 'good' : instance.health === 'unhealthy' ? 'bad' : 'neutral'}>
          {instance.state}
        </Badge>
      </div>

      {error && (
        <div className="banner banner-warn">
          <Icon name="alert" /> {error}
        </div>
      )}

      <div className="card-grid">
        <div className="card stat">
          <span className="stat-head">
            <Icon name="nodes" />
            <span className="stat-label">Node</span>
          </span>
          <span className="stat-value">{instance.node}</span>
        </div>
        <div className="card stat">
          <span className="stat-head">
            <Icon name="activity" />
            <span className="stat-label">Health</span>
          </span>
          <Badge tone={instance.health === 'healthy' ? 'good' : instance.health === 'unhealthy' ? 'bad' : 'neutral'}>
            {instance.health}
          </Badge>
        </div>
        <div className="card stat">
          <span className="stat-head">
            <Icon name="stacks" />
            <span className="stat-label">Image</span>
          </span>
          <span className="mono" style={{ fontSize: 13 }}>
            {instance.image ?? '-'}
          </span>
        </div>
        <div className="card stat">
          <span className="stat-head">
            <Icon name="lock" />
            <span className="stat-label">Digest</span>
          </span>
          <span className="mono" style={{ fontSize: 12 }}>
            {instance.image_digest ?? '-'}
          </span>
        </div>
      </div>

      {isAdmin && (
        <div className="section card">
          <div className="section-head">
            <h2>
              <Icon name="settings" /> Actions
            </h2>
          </div>
          <div className="btn-row" style={{ marginBottom: 12 }}>
            <button disabled={busy} onClick={() => run(() => api.restartInstance(id))}>
              <Icon name="restart" /> Restart
            </button>
            {instance.desired === 'running' ? (
              <button disabled={busy} onClick={() => run(() => api.setDesired(id, 'stopped'))}>
                <Icon name="stop" /> Stop
              </button>
            ) : (
              <button disabled={busy} onClick={() => run(() => api.setDesired(id, 'running'))}>
                <Icon name="play" /> Start
              </button>
            )}
            {isLatest && (
              <button disabled={busy} onClick={() => run(() => api.updateInstance(id))}>
                <Icon name="download" /> Update to latest
              </button>
            )}
          </div>
          <div className="flex-row">
            <select value={moveTarget} onChange={(e) => setMoveTarget(e.target.value)} aria-label="Move to node">
              <option value="">Move to...</option>
              {eligible.map((n) => (
                <option key={n.name} value={n.name}>
                  {n.name}
                </option>
              ))}
            </select>
            <button disabled={busy || !moveTarget} onClick={() => run(() => api.moveInstance(id, moveTarget))}>
              <Icon name="move" /> Move
            </button>
            {eligible.length === 0 && <span className="faint">no eligible nodes</span>}
          </div>
        </div>
      )}

      <div className="section">
        <div className="segmented" role="tablist" style={{ marginBottom: 12 }}>
          <button role="tab" aria-selected={tab === 'logs'} onClick={() => setTab('logs')}>
            <Icon name="terminal" /> Logs
          </button>
          <button role="tab" aria-selected={tab === 'stats'} onClick={() => setTab('stats')}>
            <Icon name="activity" /> Stats
          </button>
        </div>
        {tab === 'logs' ? <InstanceLogs id={id} /> : <InstanceStats id={id} />}
      </div>
    </div>
  );
}

function InstanceLogs({ id }: { id: string }) {
  const [follow, setFollow] = useState(true);
  const [tail, setTail] = useState(200);
  const [search, setSearch] = useState('');
  const url = `/api/instances/${encodeURIComponent(id)}/logs?follow=${follow}&tail=${tail}`;
  const { items, connected } = useEventSource<LogLine>(url, { limit: 2000 });

  const visible = search ? items.filter((l) => l.message.includes(search)) : items;

  return (
    <div>
      <div className="btn-row" style={{ marginBottom: 8 }}>
        <label className="flex-row">
          <input type="checkbox" checked={follow} onChange={(e) => setFollow(e.target.checked)} />
          Follow
        </label>
        <label className="flex-row">
          Tail
          <input
            type="number"
            style={{ width: 80 }}
            value={tail}
            min={1}
            onChange={(e) => setTail(Number(e.target.value))}
          />
        </label>
        <input
          type="search"
          placeholder="search"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          aria-label="Search logs"
        />
        <span className="faint flex-row">
          <span className={`live-dot ${connected ? 'on' : 'off'}`} />
          {connected ? 'streaming' : 'connecting...'}
        </span>
      </div>
      <div className="log-view" aria-live="polite">
        {visible.map((line, i) => (
          <div key={i}>
            <span className="faint">{line.timestamp} </span>
            <span className={line.stream === 'stderr' ? 'diff-remove' : undefined}>{line.message}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

function InstanceStats({ id }: { id: string }) {
  const { items, connected } = useEventSource<InstanceStatsSample>(
    `/api/instances/${encodeURIComponent(id)}/stats`,
    { limit: 60 },
  );
  const latest = items[items.length - 1];

  if (!latest) {
    return (
      <p className="loading">
        <span className="spinner" /> {connected ? 'waiting for samples...' : 'connecting...'}
      </p>
    );
  }

  return (
    <div className="card-grid">
      <div className="card stat">
        <span className="stat-head">
          <Icon name="cpu" />
          <span className="stat-label">CPU</span>
        </span>
        <span className="stat-value">{latest.cpu_percent.toFixed(1)}%</span>
      </div>
      <div className="card stat">
        <span className="stat-head">
          <Icon name="memory" />
          <span className="stat-label">Memory</span>
        </span>
        <span className="stat-value">
          {formatBytes(latest.mem_used_bytes)} / {formatBytes(latest.mem_limit_bytes)}
        </span>
      </div>
      <div className="card stat">
        <span className="stat-head">
          <Icon name="network" />
          <span className="stat-label">Network</span>
        </span>
        <span className="stat-value" style={{ fontSize: 16 }}>
          rx {formatBytes(latest.net_rx_bytes)} / tx {formatBytes(latest.net_tx_bytes)}
        </span>
      </div>
      <div className="card stat">
        <span className="stat-head">
          <Icon name="drives" />
          <span className="stat-label">Disk IO</span>
        </span>
        <span className="stat-value" style={{ fontSize: 16 }}>
          r {formatBytes(latest.block_read_bytes)} / w {formatBytes(latest.block_write_bytes)}
        </span>
      </div>
    </div>
  );
}
