import { useEffect, useState } from 'react';
import { api } from '../api/client';
import type { ClusterStatus, InstanceState } from '../api/types';
import { useClusterState } from '../state/store';
import { Badge } from '../components/Badge';
import { Icon, type IconName } from '../components/Icon';
import { Donut, type DonutSegment } from '../components/Donut';

export function Overview() {
  const cluster = useClusterState();
  const [status, setStatus] = useState<ClusterStatus | null>(null);
  const [instanceStates, setInstanceStates] = useState<InstanceState[] | null>(null);

  useEffect(() => {
    let cancelled = false;
    const load = () => api.status().then((s) => !cancelled && setStatus(s)).catch(() => {});
    load();
    // Version bumps arrive over the event stream; polling status here is just
    // a cheap way to keep quorum and leadership fresh, which the state event
    // doesn't carry.
    const id = setInterval(load, 10_000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    const load = () => api.listInstances().then((list) => !cancelled && setInstanceStates(list)).catch(() => {});
    load();
    const id = setInterval(load, 5000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, []);

  if (!cluster) {
    return (
      <p className="loading">
        <span className="spinner" /> Connecting to cluster...
      </p>
    );
  }

  const nodes = Object.values(cluster.nodes);
  const unreachable = cluster.unreachable;
  const fenced = nodes.filter((n) => unreachable.includes(n.name));
  const hasQuorumProblem = status ? !status.has_quorum : false;

  const instances = Object.values(cluster.assignments);
  const volumes = Object.values(cluster.volumes);
  const outOfSyncVolumes = volumes.filter((v) => v.generation !== (v.replicas[v.primary] ?? -1));

  const managers = nodes.filter((n) => n.role === 'manager');
  const workers = nodes.filter((n) => n.role === 'worker');
  const fencedNames = new Set(fenced.map((n) => n.name));

  const nodeSegmentsAll: DonutSegment[] = [
    { label: 'Managers active', value: managers.filter((n) => n.availability === 'active' && !fencedNames.has(n.name)).length, tone: 'good' },
    { label: 'Managers draining', value: managers.filter((n) => n.availability === 'drain' && !fencedNames.has(n.name)).length, tone: 'warn' },
    { label: 'Managers decommissioned', value: managers.filter((n) => n.availability === 'decommissioned' && !fencedNames.has(n.name)).length, tone: 'neutral' },
    { label: 'Managers unreachable', value: managers.filter((n) => fencedNames.has(n.name)).length, tone: 'bad' },
    { label: 'Workers active', value: workers.filter((n) => n.availability === 'active' && !fencedNames.has(n.name)).length, tone: 'secondary' },
    { label: 'Workers draining', value: workers.filter((n) => n.availability === 'drain' && !fencedNames.has(n.name)).length, tone: 'warn' },
    { label: 'Workers decommissioned', value: workers.filter((n) => n.availability === 'decommissioned' && !fencedNames.has(n.name)).length, tone: 'neutral' },
    { label: 'Workers unreachable', value: workers.filter((n) => fencedNames.has(n.name)).length, tone: 'bad' },
  ];
  const nodeSegments = nodeSegmentsAll.filter((s) => s.value > 0);

  const migrating = instances.filter((i) => i.migration).length;
  const instanceSegmentsAll: DonutSegment[] = [
    { label: 'Running', value: instanceStates?.filter((i) => i.state === 'running').length ?? 0, tone: 'good' },
    { label: 'Stopped', value: instanceStates?.filter((i) => i.state === 'stopped').length ?? 0, tone: 'neutral' },
    { label: 'Unknown', value: instanceStates?.filter((i) => i.state === 'unknown').length ?? 0, tone: 'warn' },
  ];
  const instanceSegments = instanceSegmentsAll.filter((s) => s.value > 0);

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="overview" />
          <div>
            <h1>Overview</h1>
            <p className="page-sub">{cluster.nodes && Object.keys(cluster.nodes).length} nodes under management</p>
          </div>
        </div>
      </div>

      {hasQuorumProblem && (
        <div className="banner banner-bad" role="alert">
          <Icon name="alert" />
          Quorum lost — the cluster cannot accept writes until enough managers are reachable.
        </div>
      )}
      {fenced.length > 0 && (
        <div className="banner banner-bad" role="alert">
          <Icon name="alert-circle" />
          {fenced.length} node{fenced.length > 1 ? 's' : ''} unreachable: {fenced.map((n) => n.name).join(', ')}
        </div>
      )}
      {!hasQuorumProblem && fenced.length === 0 && (
        <div className="banner banner-good">
          <Icon name="check-circle" />
          Cluster healthy, quorum held.
        </div>
      )}

      <div className="card-grid">
        <Stat icon="crown" label="Leader" value={status?.leader ?? '-'} small={!!status?.leader} />
        <Stat
          icon="check-circle"
          label="Quorum"
          value={status ? `${status.managers_reachable}/${status.managers_total}` : '-'}
          tone={hasQuorumProblem ? 'bad' : 'good'}
        />
        <Stat icon="nodes" label="Nodes" value={String(nodes.length)} />
        <Stat icon="stacks" label="Stacks" value={String(Object.keys(cluster.stacks).length)} />
        <Stat icon="instances" label="Instances" value={String(instances.length)} />
        <Stat icon="drives" label="Volumes" value={String(volumes.length)} />
      </div>

      {(nodeSegments.length > 0 || instanceSegments.length > 0) && (
        <div className="card-grid card-grid-2">
          {nodeSegments.length > 0 && (
            <div className="card">
              <div className="section-head">
                <h2>
                  <Icon name="nodes" /> Node availability
                </h2>
              </div>
              <Donut segments={nodeSegments} centerLabel={String(nodes.length)} />
            </div>
          )}
          {instanceSegments.length > 0 && (
            <div className="card">
              <div className="section-head">
                <h2>
                  <Icon name="instances" /> Instance state
                </h2>
              </div>
              <Donut segments={instanceSegments} centerLabel={String(instances.length)} />
              {migrating > 0 && (
                <p className="faint" style={{ margin: 'var(--space-3) 0 0' }}>
                  {migrating} migrating right now
                </p>
              )}
            </div>
          )}
        </div>
      )}

      <div className="section">
        <div className="section-head">
          <h2>
            <Icon name="nodes" /> Node health
          </h2>
        </div>
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Node</th>
                <th>Role</th>
                <th>Availability</th>
                <th>Reachable</th>
              </tr>
            </thead>
            <tbody>
              {nodes.map((n) => (
                <tr key={n.name}>
                  <td>{n.name}</td>
                  <td>{n.role}</td>
                  <td>
                    <Badge tone={n.availability === 'active' ? 'good' : n.availability === 'drain' ? 'warn' : 'neutral'}>
                      {n.availability}
                    </Badge>
                  </td>
                  <td>
                    <Badge tone={unreachable.includes(n.name) ? 'bad' : 'good'}>
                      {unreachable.includes(n.name) ? 'unreachable' : 'reachable'}
                    </Badge>
                  </td>
                </tr>
              ))}
              {nodes.length === 0 && (
                <tr>
                  <td colSpan={4} className="empty-cell">
                    no nodes yet
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>

      {outOfSyncVolumes.length > 0 && (
        <div className="section">
          <div className="section-head">
            <h2>
              <Icon name="alert" /> Degraded volumes
            </h2>
          </div>
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Volume</th>
                  <th>Primary</th>
                  <th>Generation</th>
                  <th>In sync</th>
                </tr>
              </thead>
              <tbody>
                {outOfSyncVolumes.map((v) => (
                  <tr key={v.id}>
                    <td>{v.id}</td>
                    <td>{v.primary}</td>
                    <td>{v.generation}</td>
                    <td>
                      {Object.entries(v.replicas).filter(([, g]) => g === v.generation).length}/
                      {Object.keys(v.replicas).length}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  );
}

function Stat({
  label,
  value,
  tone,
  icon,
  small,
}: {
  label: string;
  value: string;
  tone?: 'good' | 'bad';
  icon: IconName;
  small?: boolean;
}) {
  return (
    <div className="card stat">
      <span className="stat-head">
        <Icon name={icon} />
        <span className="stat-label">{label}</span>
      </span>
      <span className={small ? 'stat-value sm' : 'stat-value'} style={tone ? { color: `var(--${tone})` } : undefined}>
        {value}
      </span>
    </div>
  );
}
