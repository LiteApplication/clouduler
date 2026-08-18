import { useState } from 'react';
import { Link } from 'react-router-dom';
import { useClusterState } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { Badge } from '../components/Badge';
import { api } from '../api/client';
import { AddNodeForm } from './AddNodeForm';
import { Icon } from '../components/Icon';

export function Nodes() {
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const [showAdd, setShowAdd] = useState(false);

  if (!cluster) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading nodes...
      </p>
    );
  }
  const nodes = Object.values(cluster.nodes);

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="nodes" />
          <div>
            <h1>Nodes</h1>
            <p className="page-sub">
              {nodes.length} node{nodes.length === 1 ? '' : 's'}
              {cluster.unreachable.length > 0 && ` · ${cluster.unreachable.length} unreachable`}
            </p>
          </div>
        </div>
        {isAdmin && (
          <button className="primary" onClick={() => setShowAdd((v) => !v)}>
            <Icon name={showAdd ? 'close' : 'plus'} />
            {showAdd ? 'Cancel' : 'Add node'}
          </button>
        )}
      </div>

      {showAdd && <AddNodeForm onDone={() => setShowAdd(false)} />}

      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Role</th>
              <th>Address</th>
              <th>Drives</th>
              <th>Availability</th>
              <th>Reachable</th>
              {isAdmin && <th>Actions</th>}
            </tr>
          </thead>
          <tbody>
            {nodes.map((n) => (
              <tr key={n.name}>
                <td>
                  <Link className="row-link" to={`/nodes/${encodeURIComponent(n.name)}`}>
                    {n.name}
                  </Link>
                </td>
                <td>{n.role}</td>
                <td className="mono">{n.rpc_addr}</td>
                <td>{Object.keys(n.drives).join(', ') || '-'}</td>
                <td>
                  <Badge tone={n.availability === 'active' ? 'good' : n.availability === 'drain' ? 'warn' : 'neutral'}>
                    {n.availability}
                  </Badge>
                </td>
                <td>
                  <Badge tone={cluster.unreachable.includes(n.name) ? 'bad' : 'good'}>
                    {cluster.unreachable.includes(n.name) ? 'no' : 'yes'}
                  </Badge>
                </td>
                {isAdmin && (
                  <td>
                    <div className="btn-row">
                      {n.availability !== 'drain' && (
                        <button onClick={() => api.setAvailability(n.name, 'drain')}>
                          <Icon name="download" /> Drain
                        </button>
                      )}
                      {n.availability !== 'active' && (
                        <button onClick={() => api.setAvailability(n.name, 'active')}>
                          <Icon name="play" /> Activate
                        </button>
                      )}
                    </div>
                  </td>
                )}
              </tr>
            ))}
            {nodes.length === 0 && (
              <tr>
                <td colSpan={isAdmin ? 7 : 6} className="empty-cell">
                  no nodes yet
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
}
