import { useEffect, useMemo, useState } from 'react';
import { api, ApiError } from '../api/client';
import type { DriveStatus } from '../api/types';
import { useClusterState } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { Badge } from '../components/Badge';
import { Icon } from '../components/Icon';
import { RelationDiagram, type RelationEdge, type RelationNode } from '../components/RelationDiagram';
import { Meter } from '../components/Meter';
import { Donut, type DonutSegment } from '../components/Donut';

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

  const modeSegmentsAll: DonutSegment[] = [
    { label: 'Replicated', value: drives.filter((d) => d.mode === 'replicated').length, tone: 'primary' },
    { label: 'Served', value: drives.filter((d) => d.mode === 'served').length, tone: 'warn' },
  ];
  const modeSegments = modeSegmentsAll.filter((s) => s.value > 0);

  const { driveNodes, hostChips, edges } = useMemo(() => {
    const driveNodes: RelationNode[] = drives.map((d) => ({
      id: d.name,
      label: d.name,
      sub: d.mode,
      icon: <Icon name="drives" />,
    }));
    const hostNames = Array.from(new Set(drives.flatMap((d) => d.nodes.map((n) => n.node)))).sort();
    const hostChips: RelationNode[] = hostNames.map((name) => ({
      id: name,
      label: name,
      icon: <Icon name="nodes" />,
      tone: drives.some((d) => d.nodes.some((n) => n.node === name && !n.reachable)) ? 'bad' : undefined,
    }));
    const edges: RelationEdge[] = drives.flatMap((d) =>
      d.nodes.map((n) => ({
        from: d.name,
        to: n.node,
        tone: !n.reachable ? 'bad' : d.served_by === n.node ? 'accent' : 'neutral',
        dashed: d.mode === 'served' && d.served_by !== n.node,
      })),
    );
    return { driveNodes, hostChips, edges };
  }, [drives]);

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

      {drives.length > 0 && (
        <div className="section">
          <div className="section-head">
            <h2>
              <Icon name="drives" /> Drive modes
            </h2>
          </div>
          <div className="card">
            <Donut segments={modeSegments} centerLabel={String(drives.length)} />
          </div>
        </div>
      )}

      {drives.length > 0 && (
        <div className="section">
          <div className="section-head">
            <h2>
              <Icon name="network" /> Drive topology
            </h2>
          </div>
          <div className="card">
            <RelationDiagram
              leftTitle="Drives"
              rightTitle="Nodes"
              left={driveNodes}
              right={hostChips}
              edges={edges}
            />
          </div>
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
                    <td style={{ minWidth: 110 }}>
                      <Meter
                        label=""
                        value={inSync}
                        max={total}
                        tone={inSync === total ? 'good' : 'warn'}
                      />
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
