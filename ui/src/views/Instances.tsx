import { useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { api } from '../api/client';
import type { InstanceState } from '../api/types';
import { Badge } from '../components/Badge';

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

  if (!instances) return <p className="muted">Loading...</p>;

  const filtered = instances.filter(
    (i) => i.id.includes(filter) || i.node.includes(filter) || (i.image ?? '').includes(filter),
  );

  return (
    <div>
      <div className="page-header">
        <h1>Instances</h1>
        <input
          type="search"
          placeholder="filter by id, node, image"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          aria-label="Filter instances"
        />
      </div>
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
                <td colSpan={5} className="muted">
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
