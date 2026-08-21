import { useEffect, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { api, ApiError } from '../api/client';
import type { LogLine, NodeAddress, NodeDetail as NodeDetailT } from '../api/types';
import { useClusterState } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { Badge } from '../components/Badge';
import { formatBytes, formatUptime } from '../lib/format';
import { useEventSource } from '../hooks/useEventSource';
import { Icon } from '../components/Icon';
import { Meter } from '../components/Meter';
import { useConfigApply, useConfigDraft } from './config/draft';
import { PendingBanner } from './config/PendingBanner';
import { NodeCard } from './config/sections';

export function NodeDetail() {
  const { name = '' } = useParams();
  const navigate = useNavigate();
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const [detail, setDetail] = useState<NodeDetailT | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showLogs, setShowLogs] = useState(false);
  const [editing, setEditing] = useState(false);
  const { original, draft, setDraft, reload } = useConfigDraft();
  const {
    errors: configErrors,
    pending,
    busy,
    apply,
    confirm: confirmConfig,
    rollback: rollbackConfig,
    clearErrors,
  } = useConfigApply(reload);
  const [rollbackAfter, setRollbackAfter] = useState(60);

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

  const configNode = draft?.nodes[name];

  const startEditing = () => {
    clearErrors();
    setEditing(true);
  };

  const cancelEditing = () => {
    // Drop just this node's staged edits, in case the operator changed their
    // mind; other unrelated staged edits elsewhere in the draft stay put.
    if (original && draft) setDraft({ ...draft, nodes: { ...draft.nodes, [name]: original.nodes[name] } });
    clearErrors();
    setEditing(false);
  };

  const save = async () => {
    if (!draft) return;
    const ok = await apply(draft, `${rollbackAfter}s`);
    if (ok) setEditing(false);
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
        {detail?.usage.local && (
          <div className="card stat">
            <span className="stat-head">
              <Icon name="clock" />
              <span className="stat-label">Uptime</span>
            </span>
            <span className="stat-value">{formatUptime(detail.usage.uptime_seconds)}</span>
          </div>
        )}
      </div>

      {detail?.usage.local && (
        <div className="section">
          <div className="section-head">
            <h2>
              <Icon name="activity" /> Resource usage
            </h2>
          </div>
          <div className="card">
            <Meter
              label={
                <>
                  <Icon name="cpu" /> CPU load (1m)
                </>
              }
              value={detail.usage.load_1m}
              max={detail.usage.cpu_count}
              display={`${detail.usage.load_1m.toFixed(2)} / ${detail.usage.cpu_count} cores`}
              tone={detail.usage.load_1m > detail.usage.cpu_count ? 'bad' : 'primary'}
            />
            <Meter
              label={
                <>
                  <Icon name="memory" /> Memory
                </>
              }
              value={detail.usage.memory_used_bytes}
              max={detail.usage.memory_total_bytes}
              display={`${formatBytes(detail.usage.memory_used_bytes)} / ${formatBytes(detail.usage.memory_total_bytes)}`}
              tone={detail.usage.memory_used_bytes / detail.usage.memory_total_bytes > 0.9 ? 'bad' : 'accent'}
            />
          </div>
        </div>
      )}

      <div className="section">
        <div className="section-head">
          <h2>
            <Icon name="settings" /> Configuration
          </h2>
          {isAdmin && !editing && configNode && (
            <button onClick={startEditing}>
              <Icon name="edit" /> Edit
            </button>
          )}
        </div>

        {pending?.armed && <PendingBanner pending={pending} onConfirm={confirmConfig} onRollback={rollbackConfig} />}

        {draft && !configNode && (
          <p className="muted">This node is not in the cluster config yet.</p>
        )}
        {!draft && (
          <p className="loading">
            <span className="spinner" /> Loading configuration...
          </p>
        )}

        {configNode && !editing && (
          <>
            <div className="card-grid">
              <div className="card stat">
                <span className="stat-head">
                  <Icon name="crown" />
                  <span className="stat-label">Role</span>
                </span>
                <span className="stat-value">{configNode.role}</span>
              </div>
              <div className="card stat">
                <span className="stat-head">
                  <Icon name="network" />
                  <span className="stat-label">RPC port</span>
                </span>
                <span className="stat-value">{configNode.rpc_port}</span>
              </div>
              <div className="card stat">
                <span className="stat-head">
                  <Icon name="drives" />
                  <span className="stat-label">Data dir</span>
                </span>
                <span className="stat-value mono">{configNode.data_dir ?? 'cluster default'}</span>
              </div>
              <div className="card stat">
                <span className="stat-head">
                  <Icon name="key" />
                  <span className="stat-label">Creates cluster key</span>
                </span>
                <span className="stat-value">
                  {configNode.generate_secret_key === null || configNode.generate_secret_key === undefined
                    ? 'follows cluster setting'
                    : configNode.generate_secret_key
                      ? 'yes'
                      : 'never'}
                </span>
              </div>
            </div>

            <div className="table-wrap">
              <table>
                <thead>
                  <tr>
                    <th>Network</th>
                    <th>Address</th>
                  </tr>
                </thead>
                <tbody>
                  {Object.entries(configNode.network).map(([net, addr]) => (
                    <tr key={net}>
                      <td>{net}</td>
                      <td className="mono">{describeAddress(addr)}</td>
                    </tr>
                  ))}
                  {Object.keys(configNode.network).length === 0 && (
                    <tr>
                      <td colSpan={2} className="empty-cell">
                        no networks configured
                      </td>
                    </tr>
                  )}
                </tbody>
              </table>
            </div>

            <div className="table-wrap" style={{ marginTop: 12 }}>
              <table>
                <thead>
                  <tr>
                    <th>Drive</th>
                    <th>Local path</th>
                  </tr>
                </thead>
                <tbody>
                  {Object.entries(configNode.drives).map(([drive, path]) => (
                    <tr key={drive}>
                      <td>{drive}</td>
                      <td className="mono">{path}</td>
                    </tr>
                  ))}
                  {Object.keys(configNode.drives).length === 0 && (
                    <tr>
                      <td colSpan={2} className="empty-cell">
                        no drives configured
                      </td>
                    </tr>
                  )}
                </tbody>
              </table>
            </div>
          </>
        )}

        {configNode && editing && draft && (
          <>
            <NodeCard
              name={name}
              node={configNode}
              networks={draft.networks}
              drives={draft.drives}
              onChange={(patch) =>
                setDraft({ ...draft, nodes: { ...draft.nodes, [name]: { ...configNode, ...patch } } })
              }
              onRemove={() => {
                if (!confirm(`Remove ${name} from the cluster config? This does not decommission it.`)) return;
                const next = { ...draft.nodes };
                delete next[name];
                setDraft({ ...draft, nodes: next });
              }}
              disabled={!isAdmin || busy}
            />
            {configErrors.length > 0 && (
              <div className="banner banner-bad" role="alert">
                <Icon name="alert" />
                <ul style={{ margin: 0, paddingLeft: 18 }}>
                  {configErrors.map((e, i) => (
                    <li key={i}>
                      {e.path && <span className="mono">{e.path}</span>}
                      {e.path && ': '}
                      {e.message}
                    </li>
                  ))}
                </ul>
              </div>
            )}
            <div className="btn-row">
              <label
                className="rollback-after"
                title="If this edit affects networks, nodes, the web ui, the reverse proxy or Cloudflare, saving arms an auto-rollback timer."
              >
                <Icon name="clock" /> Rollback after
                <input
                  type="number"
                  min={5}
                  max={3600}
                  value={rollbackAfter}
                  onChange={(e) => setRollbackAfter(Math.max(5, Number(e.target.value) || 60))}
                  disabled={busy}
                />
                s
              </label>
              <button onClick={cancelEditing} disabled={busy}>
                <Icon name="restart" /> Cancel
              </button>
              <button className="primary" onClick={save} disabled={busy}>
                <Icon name="upload" /> Save
              </button>
            </div>
          </>
        )}
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

/** Read-only one-line summary of a node's address on one network. */
function describeAddress(addr: NodeAddress): string {
  if (typeof addr === 'string') return addr;
  const ports = addr.open_ports.map((p) => `${p.port}/${p.protocol}`).join(', ');
  if ('interface' in addr) {
    return ports ? `${addr.interface} (open: ${ports})` : addr.interface;
  }
  const parts = [addr.ipv4, addr.ipv6, addr.domain].filter((v): v is string => !!v);
  const base = parts.length > 0 ? parts.join(', ') : '(no address set)';
  return ports ? `${base} — open: ${ports}` : base;
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
