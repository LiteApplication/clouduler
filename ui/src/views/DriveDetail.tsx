import { useEffect, useState } from 'react';
import { useParams } from 'react-router-dom';
import { api, ApiError } from '../api/client';
import type { DriveStatus } from '../api/types';
import { useClusterState } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { Badge } from '../components/Badge';
import { Icon } from '../components/Icon';
import { Meter } from '../components/Meter';

export function DriveDetail() {
  const { name = '' } = useParams();
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const [drive, setDrive] = useState<DriveStatus | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    const load = () =>
      api
        .listDrives()
        .then((all) => !cancelled && setDrive(all.find((d) => d.name === name) ?? null))
        .catch((e) => !cancelled && setError(e instanceof ApiError ? e.message : String(e)));
    load();
    const id = setInterval(load, 10_000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [name]);

  const sync = async (volume: string) => {
    try {
      await api.syncVolume(name, volume);
    } catch (e) {
      alert(e instanceof ApiError ? e.message : 'sync failed');
    }
  };

  const volumes = cluster
    ? Object.values(cluster.volumes).filter((v) => v.id.startsWith(`${name}/`))
    : [];

  if (error) {
    return (
      <p className="field-error">
        <Icon name="alert-circle" /> {error}
      </p>
    );
  }

  if (!drive) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading drive...
      </p>
    );
  }

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="drives" />
          <div>
            <h1>{drive.name}</h1>
            <p className="page-sub">
              {drive.mode} · sync every {drive.sync_interval}
            </p>
          </div>
        </div>
      </div>

      <div className="card-grid">
        <div className="card stat">
          <span className="stat-head">
            <Icon name="drives" />
            <span className="stat-label">Mode</span>
          </span>
          <Badge tone={drive.mode === 'served' ? 'warn' : 'neutral'}>{drive.mode}</Badge>
        </div>
        {drive.mode === 'served' && (
          <div className="card stat">
            <span className="stat-head">
              <Icon name="nodes" />
              <span className="stat-label">Active server</span>
            </span>
            <span className="stat-value">{drive.active_server ?? 'none reachable'}</span>
          </div>
        )}
        <div className="card stat">
          <span className="stat-head">
            <Icon name="stacks" />
            <span className="stat-label">Volumes</span>
          </span>
          <span className="stat-value">{volumes.length}</span>
        </div>
      </div>

      <div className="section">
        <div className="section-head">
          <h2>
            <Icon name="nodes" /> Implementing nodes
          </h2>
        </div>
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Node</th>
                <th>Reachable</th>
                {drive.mode === 'served' && <th>Priority</th>}
              </tr>
            </thead>
            <tbody>
              {drive.nodes.map((n) => (
                <tr key={n.node}>
                  <td>{n.node}</td>
                  <td>
                    <Badge tone={n.reachable ? 'good' : 'bad'}>{n.reachable ? 'yes' : 'no'}</Badge>
                  </td>
                  {drive.mode === 'served' && <td>{drive.served_by.indexOf(n.node) + 1 || '-'}</td>}
                </tr>
              ))}
              {drive.nodes.length === 0 && (
                <tr>
                  <td colSpan={drive.mode === 'served' ? 3 : 2} className="empty-cell">
                    no implementing nodes
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>

      <div className="section">
        <div className="section-head">
          <h2>
            <Icon name="stacks" /> Volumes
          </h2>
        </div>
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Volume</th>
                <th>Generation</th>
                <th>Primary</th>
                <th>In sync</th>
                <th>Replicas</th>
                {isAdmin && <th>Actions</th>}
              </tr>
            </thead>
            <tbody>
              {volumes.map((v) => {
                const volume = v.id.split('/')[1];
                const inSync = Object.entries(v.replicas).filter(([, g]) => g === v.generation).length;
                const total = Object.keys(v.replicas).length;
                return (
                  <tr key={v.id}>
                    <td>{volume}</td>
                    <td>{v.generation}</td>
                    <td>{v.primary}</td>
                    <td style={{ minWidth: 110 }}>
                      <Meter label="" value={inSync} max={total} tone={inSync === total ? 'good' : 'warn'} />
                    </td>
                    <td className="wrap">
                      {Object.entries(v.replicas)
                        .map(([node, gen]) => `${node}@${gen}`)
                        .join(', ')}
                    </td>
                    {isAdmin && (
                      <td>
                        <button onClick={() => sync(volume)}>
                          <Icon name="sync" /> Sync now
                        </button>
                      </td>
                    )}
                  </tr>
                );
              })}
              {volumes.length === 0 && (
                <tr>
                  <td colSpan={isAdmin ? 6 : 5} className="empty-cell">
                    no volumes on this drive
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
