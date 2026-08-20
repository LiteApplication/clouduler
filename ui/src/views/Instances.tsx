import { useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { api } from '../api/client';
import type { InstanceState } from '../api/types';
import { Badge } from '../components/Badge';
import { Icon } from '../components/Icon';
import { Donut, type DonutSegment } from '../components/Donut';

export function Instances() {
  const [instances, setInstances] = useState<InstanceState[] | null>(null);
  const [filter, setFilter] = useState('');

  useEffect(() => {
    let cancelled = false;
    const load = () => api.listInstances().then((list) => !cancelled && setInstances(list));
    load();
    const id = setInterval(load, 5000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, []);

  if (!instances) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading instances...
      </p>
    );
  }

  const filtered = instances.filter(
    (i) => i.id.includes(filter) || i.node.includes(filter) || (i.image ?? '').includes(filter),
  );

  const stateSegmentsAll: DonutSegment[] = [
    { label: 'Running', value: instances.filter((i) => i.state === 'running').length, tone: 'good' },
    { label: 'Stopped', value: instances.filter((i) => i.state === 'stopped').length, tone: 'neutral' },
    { label: 'Unknown', value: instances.filter((i) => i.state === 'unknown').length, tone: 'warn' },
  ];
  const stateSegments = stateSegmentsAll.filter((s) => s.value > 0);

  const healthSegmentsAll: DonutSegment[] = [
    { label: 'Healthy', value: instances.filter((i) => i.health === 'healthy').length, tone: 'good' },
    { label: 'Unhealthy', value: instances.filter((i) => i.health === 'unhealthy').length, tone: 'bad' },
    { label: 'Starting', value: instances.filter((i) => i.health === 'starting').length, tone: 'warn' },
    { label: 'No healthcheck', value: instances.filter((i) => i.health === 'none').length, tone: 'neutral' },
  ];
  const healthSegments = healthSegmentsAll.filter((s) => s.value > 0);

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="instances" />
          <div>
            <h1>Instances</h1>
            <p className="page-sub">
              {instances.filter((i) => i.state === 'running').length} of {instances.length} running
            </p>
          </div>
        </div>
        <div className="flex-row" style={{ flexWrap: 'nowrap', maxWidth: 300, flex: '1 1 220px' }}>
          <Icon name="search" style={{ color: 'var(--text-faint)' }} />
          <input
            type="search"
            placeholder="filter by id, node, image"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            aria-label="Filter instances"
          />
        </div>
      </div>

      {instances.length > 0 && (
        <div className="card-grid card-grid-2">
          <div className="card">
            <div className="section-head">
              <h2>
                <Icon name="instances" /> By state
              </h2>
            </div>
            <Donut segments={stateSegments} centerLabel={String(instances.length)} />
          </div>
          <div className="card">
            <div className="section-head">
              <h2>
                <Icon name="activity" /> By health
              </h2>
            </div>
            <Donut segments={healthSegments} centerLabel={String(instances.length)} />
          </div>
        </div>
      )}

      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Instance</th>
              <th>Node</th>
              <th>State</th>
              <th>Health</th>
              <th>Image</th>
            </tr>
          </thead>
          <tbody>
            {filtered.map((i) => (
              <tr key={i.id}>
                <td>
                  <Link className="row-link" to={`/instances/${encodeURIComponent(i.id)}`}>
                    {i.id}
                  </Link>
                </td>
                <td>{i.node}</td>
                <td>
                  <Badge tone={i.state === 'running' ? 'good' : i.health === 'unhealthy' ? 'bad' : 'neutral'}>
                    {i.state}
                  </Badge>
                </td>
                <td>
                  <Badge tone={i.health === 'healthy' ? 'good' : i.health === 'unhealthy' ? 'bad' : 'neutral'}>
                    {i.health}
                  </Badge>
                </td>
                <td className="mono">{i.image ?? '-'}</td>
              </tr>
            ))}
            {filtered.length === 0 && (
              <tr>
                <td colSpan={5} className="empty-cell">
                  no instances match
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
}
