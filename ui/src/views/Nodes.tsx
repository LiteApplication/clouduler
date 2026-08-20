import { useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import { useClusterState } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { Badge } from '../components/Badge';
import { api } from '../api/client';
import { AddNodeForm } from './AddNodeForm';
import { Icon } from '../components/Icon';
import { RelationDiagram, type RelationEdge, type RelationNode } from '../components/RelationDiagram';

export function Nodes() {
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const [showAdd, setShowAdd] = useState(false);

  const nodes = useMemo(() => Object.values(cluster?.nodes ?? {}), [cluster]);
  const unreachable = cluster?.unreachable ?? [];

  const { networkNodes, nodeChips, edges } = useMemo(() => {
    const networkNames = Array.from(new Set(nodes.flatMap((n) => n.networks))).sort();
    const networkNodes: RelationNode[] = networkNames.map((name) => ({
      id: name,
      label: name,
      icon: <Icon name="network" />,
    }));
    const nodeChips: RelationNode[] = nodes.map((n) => ({
      id: n.name,
      label: n.name,
      sub: n.role,
      icon: <Icon name="nodes" />,
      tone: unreachable.includes(n.name) ? 'bad' : n.availability === 'drain' ? 'warn' : undefined,
    }));
    const edges: RelationEdge[] = nodes.flatMap((n) =>
      n.networks.map((net) => ({
        from: net,
        to: n.name,
        tone: unreachable.includes(n.name) ? 'bad' : n.egress_networks.includes(net) ? 'accent' : 'neutral',
      })),
    );
    return { networkNodes, nodeChips, edges };
  }, [nodes, unreachable]);

  if (!cluster) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading nodes...
      </p>
    );
  }

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

      {networkNodes.length > 0 && (
        <div className="section">
          <div className="section-head">
            <h2>
              <Icon name="network" /> Network map
            </h2>
          </div>
          <div className="card">
            <RelationDiagram
              leftTitle="Networks"
              rightTitle="Nodes"
              left={networkNodes}
              right={nodeChips}
              edges={edges}
            />
          </div>
        </div>
      )}

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
