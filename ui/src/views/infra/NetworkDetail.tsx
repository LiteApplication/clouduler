// /networks/:name: one network's settings and the nodes on it.

import { useEffect, useState } from 'react';
import { useParams } from 'react-router-dom';
import { api, ApiError } from '../../api/client';
import type { ClusterConfig } from '../../api/types';
import { Banner, KV, PageHeader, Panel, Tag } from '../../ds';
import { useClusterState } from '../../state/store';
import { describeAddress } from './NodeDetail';
import { DataTable } from './table';

export function NetworkDetail() {
  const { name = '' } = useParams();
  const cluster = useClusterState();
  const [config, setConfig] = useState<ClusterConfig | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let off = false;
    api
      .getConfiguration()
      .then((c) => !off && setConfig(c))
      .catch((e) => !off && setError(e instanceof ApiError ? e.message : String(e)));
    return () => {
      off = true;
    };
  }, []);

  if (error) return <Banner tone="danger">{error}</Banner>;
  if (!config) return <p className="cl-muted">Loading network…</p>;
  const net = config.networks[name];
  if (!net) return <Banner tone="warn" title={`No network named ${name}`}>Check the name under Settings, Network.</Banner>;

  const onNet = Object.entries(config.nodes).filter(([, node]) => name in node.network);

  return (
    <>
      <PageHeader breadcrumb={[{ label: 'Networks' }, { label: name }]} title={name} sub={net.type} />
      <div className="stack">
        <Panel title="About">
          <KV items={[
            ['Type', net.type],
            ['Subnet', net.subnet ?? 'none'],
            ['Domain', net.domain ?? 'none'],
            ['Nodes', onNet.length],
          ]} />
        </Panel>
        <Panel title="Nodes on this network" flush>
          <DataTable
            caption="Nodes on this network"
            rows={onNet.map(([n, node]) => ({ id: n, name: n, address: describeAddress(node.network[name]), down: !!cluster?.unreachable.includes(n) }))}
            empty="No node is configured on this network."
            columns={[
              { key: 'name', header: 'Node', render: (r) => <strong>{r.name}</strong> },
              { key: 'addr', header: 'Address', mono: true, render: (r) => r.address },
              { key: 'reach', header: 'Reachable', render: (r) => <Tag tone={r.down ? 'danger' : 'ok'}>{r.down ? 'no' : 'yes'}</Tag> },
            ]}
          />
        </Panel>
      </div>
    </>
  );
}
