import { useEffect, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { api, ApiError } from '../api/client';
import type { NodeDetail as NodeDetailT, LogLine } from '../api/types';
import { useClusterState } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { Badge } from '../components/Badge';
import { formatBytes, formatUptime } from '../lib/format';
import { useEventSource } from '../hooks/useEventSource';
import { Icon } from '../components/Icon';

export function NodeDetail() {
  const { name = '' } = useParams();
  const navigate = useNavigate();
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const [detail, setDetail] = useState<NodeDetailT | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showLogs, setShowLogs] = useState(false);

  useEffect(() => {
    let cancelled = false;
    const load = () =>
      api
        .getNode(name)
        .then((d) => !cancelled && setDetail(d))
        .catch((e) => !cancelled && setError(e instanceof ApiError ? e.message : String(e)));
    load();
    const id = setInterval(load, 5000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [name]);

  const node = cluster?.nodes[name];
  const instances = cluster ? Object.values(cluster.assignments).filter((a) => a.node === name) : [];

  const decommission = async () => {
    if (!confirm(`Decommission ${name}? It must not be running any work.`)) return;
    try {
      await api.removeNode(name);
      navigate('/nodes');
    } catch (e) {
      alert(e instanceof ApiError ? e.message : 'failed to decommission');
    }
  };

  if (!node) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading node...
      </p>
    );
  }

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="nodes" />
          <div>
            <h1>{name}</h1>
            <p className="page-sub mono">{node.rpc_addr}</p>
          </div>
        </div>
        {isAdmin && (
          <div className="btn-row">
            {node.availability !== 'drain' && (
              <button onClick={() => api.setAvailability(name, 'drain')}>
                <Icon name="download" /> Drain
              </button>
            )}
            {node.availability !== 'active' && (
              <button onClick={() => api.setAvailability(name, 'active')}>
                <Icon name="play" /> Activate
              </button>
            )}
            <button className="danger" onClick={decommission} disabled={instances.length > 0}>
              <Icon name="trash" /> Decommission
            </button>
          </div>
        )}
      </div>

      {error && (
        <div className="banner banner-warn">
          <Icon name="alert" /> {error}
        </div>
      )}

      <div className="card-grid">
        <div className="card stat">
          <span className="stat-head">
            <Icon name="crown" />
            <span className="stat-label">Role</span>
          </span>
          <span className="stat-value">{node.role}</span>
        </div>
        <div className="card stat">
          <span className="stat-head">
            <Icon name="activity" />
            <span className="stat-label">Availability</span>
          </span>
          <Badge tone={node.availability === 'active' ? 'good' : node.availability === 'drain' ? 'warn' : 'neutral'}>
            {node.availability}
          </Badge>
        </div>
        <div className="card stat">
          <span className="stat-head">
            <Icon name="network" />
            <span className="stat-label">Reachable</span>
          </span>
          <Badge tone={cluster?.unreachable.includes(name) ? 'bad' : 'good'}>
            {cluster?.unreachable.includes(name) ? 'no' : 'yes'}
          </Badge>
        </div>
        <div className="card stat">
          <span className="stat-head">
            <Icon name="instances" />
            <span className="stat-label">Instances</span>
          </span>
          <span className="stat-value">{instances.length}</span>
        </div>
        {detail && (
          <>
            <div className="card stat">
              <span className="stat-head">
                <Icon name="cpu" />
                <span className="stat-label">CPU</span>
              </span>
              <span className="stat-value">
                {detail.load_avg_1.toFixed(2)} / {detail.cpu_count}
              </span>
            </div>
            <div className="card stat">
              <span className="stat-head">
                <Icon name="memory" />
                <span className="stat-label">Memory</span>
              </span>
              <span className="stat-value">
                {formatBytes(detail.mem_used_bytes)} / {formatBytes(detail.mem_total_bytes)}
              </span>
            </div>
            <div className="card stat">
              <span className="stat-head">
                <Icon name="clock" />
                <span className="stat-label">Uptime</span>
              </span>
              <span className="stat-value">{formatUptime(detail.uptime_secs)}</span>
            </div>
          </>
        )}
      </div>

      <div className="section">
        <div className="section-head">
          <h2>
            <Icon name="drives" /> Drives
          </h2>
        </div>
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Drive</th>
                <th>Local path</th>
                <th>Disk usage</th>
              </tr>
            </thead>
            <tbody>
              {Object.entries(node.drives).map(([drive, path]) => {
                const disk = detail?.disks.find((d) => d.drive === drive);
                return (
                  <tr key={drive}>
                    <td>{drive}</td>
                    <td className="mono">{path}</td>
                    <td>{disk ? `${formatBytes(disk.used_bytes)} / ${formatBytes(disk.total_bytes)}` : '-'}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      </div>

      <div className="section">
        <div className="section-head">
          <h2>
            <Icon name="network" /> Addresses
          </h2>
        </div>
        <div className="card mono">{node.rpc_addr}</div>
      </div>

      <div className="section">
        <div className="section-head">
          <h2>
            <Icon name="terminal" /> Daemon logs
          </h2>
          <button onClick={() => setShowLogs((v) => !v)}>
            <Icon name={showLogs ? 'eye-off' : 'eye'} />
            {showLogs ? 'Hide' : 'Show'}
          </button>
        </div>
        {showLogs && <NodeLogs name={name} />}
      </div>
    </div>
  );
}

function NodeLogs({ name }: { name: string }) {
  const { items, connected } = useEventSource<LogLine>(`/api/nodes/${encodeURIComponent(name)}/logs?follow=true`, {
    limit: 500,
  });
  return (
    <div>
      <p className="faint flex-row">
        <span className={`live-dot ${connected ? 'on' : 'off'}`} />
        {connected ? 'streaming' : 'connecting...'}
      </p>
      <div className="log-view" aria-live="polite">
        {items.map((line, i) => (
          <div key={i}>
            <span className="faint">{line.timestamp} </span>
            {line.message}
          </div>
        ))}
      </div>
    </div>
  );
}
