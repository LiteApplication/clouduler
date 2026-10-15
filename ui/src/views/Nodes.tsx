import { useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import { useClusterState } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { Badge } from '../components/Badge';
import { api, ApiError } from '../api/client';
import { AddNodeForm } from './AddNodeForm';
import { Icon } from '../components/Icon';
import { NodeGraph } from '../components/NodeGraph';
import { RelationDiagram, type RelationEdge, type RelationNode } from '../components/RelationDiagram';
import { useConfigDraft } from './config/draft';

export function Nodes() {
  const cluster = useClusterState();
  const { original: config } = useConfigDraft();
  const { isAdmin } = useAuth();
  const [showAdd, setShowAdd] = useState(false);
  const [rebalancing, setRebalancing] = useState(false);

  const rebalance = async () => {
    setRebalancing(true);
    try {
      const result = await api.rebalance();
      if (result.moves.length === 0) {
        alert('already balanced, nothing to move');
      } else {
        alert(
          `moving ${result.moves.length} instance${result.moves.length === 1 ? '' : 's'}:\n` +
            result.moves.map((m) => `${m.instance}: ${m.from} → ${m.to}`).join('\n'),
        );
      }
    } catch (e) {
      alert(e instanceof ApiError ? e.message : 'failed to rebalance');
    } finally {
      setRebalancing(false);
    }
  };

  const nodes = useMemo(() => Object.values(cluster?.nodes ?? {}), [cluster]);
  const unreachable = cluster?.unreachable ?? [];

  // The live roster only ever holds a node once the leader has managed to
  // reach it (see `seed_roster` in the daemon), so a node that is declared in
  // the config but not yet reachable — still joining, or currently down —
  // would otherwise be indistinguishable from a name nobody has configured.
  // Listing it here from the config, flagged as pending, is what tells an
  // operator "this is already taken" instead of just leaving it invisible.
  const pendingNames = useMemo(
    () => Object.keys(config?.nodes ?? {}).filter((name) => !cluster?.nodes[name]),
    [config, cluster],
  );

  const { networkNodes, nodeChips, edges } = useMemo(() => {
    const pendingNetworks = pendingNames.flatMap((name) => Object.keys(config?.nodes[name]?.network ?? {}));
    const networkNames = Array.from(new Set([...nodes.flatMap((n) => n.networks), ...pendingNetworks])).sort();
    const networkNodes: RelationNode[] = networkNames.map((name) => ({
      id: name,
      label: name,
      icon: <Icon name="network" />,
    }));
    const nodeChips: RelationNode[] = [
      ...nodes.map((n) => ({
        id: n.name,
        label: n.name,
        sub: n.role,
        icon: <Icon name="nodes" />,
        tone: (unreachable.includes(n.name) ? 'bad' : n.availability === 'drain' ? 'warn' : undefined) as
          | 'bad'
          | 'warn'
          | undefined,
      })),
      ...pendingNames.map((name) => ({
        id: name,
        label: name,
        sub: config?.nodes[name]?.role,
        icon: <Icon name="nodes" />,
        tone: 'warn' as const,
      })),
    ];
    const edges: RelationEdge[] = [
      ...nodes.flatMap((n) =>
        n.networks.map((net) => ({
          from: net,
          to: n.name,
          tone: (unreachable.includes(n.name) ? 'bad' : n.egress_networks.includes(net) ? 'accent' : 'neutral') as
            | 'bad'
            | 'accent'
            | 'neutral',
        })),
      ),
      ...pendingNames.flatMap((name) =>
        Object.keys(config?.nodes[name]?.network ?? {}).map((net) => ({
          from: net,
          to: name,
          tone: 'bad' as const,
        })),
      ),
    ];
    return { networkNodes, nodeChips, edges };
  }, [nodes, unreachable, pendingNames, config]);

  if (!cluster) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading nodes...
      </p>
    );
  }

  const total = nodes.length + pendingNames.length;

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="nodes" />
          <div>
            <h1>Nodes</h1>
            <p className="page-sub">
              {total} node{total === 1 ? '' : 's'}
              {cluster.unreachable.length > 0 && ` · ${cluster.unreachable.length} unreachable`}
              {pendingNames.length > 0 && ` · ${pendingNames.length} pending`}
            </p>
          </div>
        </div>
        {isAdmin && (
          <div className="btn-row">
            <button onClick={rebalance} disabled={rebalancing} title="Move instances around to smooth load across nodes">
              <Icon name="refresh" />
              {rebalancing ? 'Rebalancing…' : 'Rebalance'}
            </button>
            <button className="primary" onClick={() => setShowAdd((v) => !v)}>
              <Icon name={showAdd ? 'close' : 'plus'} />
              {showAdd ? 'Cancel' : 'Add node'}
            </button>
          </div>
        )}
      </div>

      {showAdd && <AddNodeForm onDone={() => setShowAdd(false)} />}

      {total > 1 && (
        <div className="section">
          <div className="section-head">
            <h2>
              <Icon name="nodes" /> Connectivity
            </h2>
          </div>
          <div className="card">
            <NodeGraph names={[...nodes.map((n) => n.name), ...pendingNames]} />
          </div>
        </div>
      )}

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
            {pendingNames.map((name) => {
              const configured = config?.nodes[name];
              return (
                <tr key={name}>
                  <td>
                    <Link className="row-link" to={`/nodes/${encodeURIComponent(name)}`}>
                      {name}
                    </Link>
                  </td>
                  <td>{configured?.role ?? '-'}</td>
                  <td className="mono">-</td>
                  <td>{Object.keys(configured?.drives ?? {}).join(', ') || '-'}</td>
                  <td>
                    <Badge tone="neutral">pending</Badge>
                  </td>
                  <td>
                    <Badge tone="bad">no</Badge>
                  </td>
                  {isAdmin && <td />}
                </tr>
              );
            })}
            {total === 0 && (
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
