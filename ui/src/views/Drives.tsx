import { useEffect, useState } from 'react';
import { api, ApiError } from '../api/client';
import type { DriveStatus } from '../api/types';
import { useClusterState } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { Badge } from '../components/Badge';
import { Icon } from '../components/Icon';

export function Drives() {
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const [drives, setDrives] = useState<DriveStatus[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    const load = () =>
      api
        .listDrives()
        .then((d) => !cancelled && setDrives(d))
        .catch((e) => !cancelled && setError(e instanceof ApiError ? e.message : String(e)));
    load();
    const id = setInterval(load, 10_000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, []);

  const sync = async (drive: string, volume: string) => {
    try {
      await api.syncVolume(drive, volume);
    } catch (e) {
      alert(e instanceof ApiError ? e.message : 'sync failed');
    }
  };

  const volumes = cluster ? Object.values(cluster.volumes) : [];

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="drives" />
          <div>
            <h1>Drives and volumes</h1>
            <p className="page-sub">
              {drives.length} drive{drives.length === 1 ? '' : 's'} · {volumes.length} volume
              {volumes.length === 1 ? '' : 's'}
            </p>
          </div>
        </div>
      </div>
      {error && (
        <div className="banner banner-warn">
          <Icon name="alert" /> {error}
        </div>
      )}

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
                <th>Mode</th>
                <th>Served by</th>
                <th>Sync interval</th>
                <th>Implementing nodes</th>
              </tr>
            </thead>
            <tbody>
              {drives.map((d) => (
                <tr key={d.name}>
                  <td>{d.name}</td>
                  <td>
                    <Badge tone={d.mode === 'served' ? 'warn' : 'neutral'}>{d.mode}</Badge>
                  </td>
                  <td>{d.served_by ?? '-'}</td>
                  <td>{d.sync_interval}</td>
                  <td className="wrap">
                    {d.nodes.length === 0
                      ? '-'
                      : d.nodes
                          .map((n) => `${n.node}${n.reachable ? '' : ' (unreachable)'}`)
                          .join(', ')}
                  </td>
                </tr>
              ))}
              {drives.length === 0 && (
                <tr>
                  <td colSpan={5} className="empty-cell">
                    no drives configured
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
                const [drive, volume] = v.id.split('/');
                const inSync = Object.entries(v.replicas).filter(([, g]) => g === v.generation).length;
                const total = Object.keys(v.replicas).length;
                return (
                  <tr key={v.id}>
                    <td>{v.id}</td>
                    <td>{v.generation}</td>
                    <td>{v.primary}</td>
                    <td>
                      <Badge tone={inSync === total ? 'good' : 'warn'}>
                        {inSync}/{total}
                      </Badge>
                    </td>
                    <td className="wrap">
                      {Object.entries(v.replicas)
                        .map(([node, gen]) => `${node}@${gen}`)
                        .join(', ')}
                    </td>
                    {isAdmin && (
                      <td>
                        <button onClick={() => sync(drive, volume)}>
                          <Icon name="sync" /> Sync now
                        </button>
                      </td>
                    )}
                  </tr>
                );
              })}
              {volumes.length === 0 && (
                <tr>
                  <td colSpan={6} className="empty-cell">
                    no volumes
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
