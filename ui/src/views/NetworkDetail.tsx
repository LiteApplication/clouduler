import { useEffect, useState } from 'react';
import { useParams } from 'react-router-dom';
import { api, ApiError } from '../api/client';
import type { ClusterConfig, NodeAddress } from '../api/types';
import { useClusterState } from '../state/store';
import { Badge } from '../components/Badge';
import { Icon } from '../components/Icon';

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

export function NetworkDetail() {
  const { name = '' } = useParams();
  const cluster = useClusterState();
  const [config, setConfig] = useState<ClusterConfig | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    api
      .getConfiguration()
      .then((c) => !cancelled && setConfig(c))
      .catch((e) => !cancelled && setError(e instanceof ApiError ? e.message : String(e)));
    return () => {
      cancelled = true;
    };
  }, []);

  if (error) {
    return (
      <p className="field-error">
        <Icon name="alert-circle" /> {error}
      </p>
    );
  }

  if (!config) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading network...
      </p>
    );
  }

  const net = config.networks[name];
  if (!net) {
    return (
      <div className="banner banner-warn">
        <Icon name="alert" /> No network named {name}.
      </div>
    );
  }

  const nodesOnNetwork = Object.entries(config.nodes).filter(([, node]) => name in node.network);

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="network" />
          <div>
            <h1>{name}</h1>
            <p className="page-sub">{net.type}</p>
          </div>
        </div>
      </div>

      <div className="card-grid">
        <div className="card stat">
          <span className="stat-head">
            <Icon name="network" />
            <span className="stat-label">Type</span>
          </span>
          <span className="stat-value">{net.type}</span>
        </div>
        <div className="card stat">
          <span className="stat-head">
            <Icon name="network" />
            <span className="stat-label">Subnet</span>
          </span>
          <span className="stat-value mono">{net.subnet ?? 'none'}</span>
        </div>
        <div className="card stat">
          <span className="stat-head">
            <Icon name="network" />
            <span className="stat-label">Domain</span>
          </span>
          <span className="stat-value">{net.domain ?? 'none'}</span>
        </div>
        <div className="card stat">
          <span className="stat-head">
            <Icon name="nodes" />
            <span className="stat-label">Nodes</span>
          </span>
          <span className="stat-value">{nodesOnNetwork.length}</span>
        </div>
      </div>

      <div className="section">
        <div className="section-head">
          <h2>
            <Icon name="nodes" /> Nodes on this network
          </h2>
        </div>
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Node</th>
                <th>Address</th>
                <th>Reachable</th>
              </tr>
            </thead>
            <tbody>
              {nodesOnNetwork.map(([nodeName, node]) => (
                <tr key={nodeName}>
                  <td>{nodeName}</td>
                  <td className="mono">{describeAddress(node.network[name])}</td>
                  <td>
                    <Badge tone={cluster?.unreachable.includes(nodeName) ? 'bad' : 'good'}>
                      {cluster?.unreachable.includes(nodeName) ? 'no' : 'yes'}
                    </Badge>
                  </td>
                </tr>
              ))}
              {nodesOnNetwork.length === 0 && (
                <tr>
                  <td colSpan={3} className="empty-cell">
                    no nodes configured on this network
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
