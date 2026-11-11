// /networks/:name: one network's settings and the nodes on it.

import { useEffect, useState } from 'react';
import { useParams } from 'react-router-dom';
import { api, ApiError } from '../../api/client';
import type { ClusterConfig, NetworkStatus } from '../../api/types';
import { Banner, KV, PageHeader, Panel, Tag } from '../../ds';
import { useClusterState } from '../../state/store';
import { describeAddress } from './NodeDetail';
import { DataTable } from './table';

function describeContainerPort(p: { port: number; protocol: string; target_port?: number | null }): string {
  return p.target_port && p.target_port !== p.port ? `${p.port}/${p.protocol} -> ${p.target_port}` : `${p.port}/${p.protocol}`;
}

function RouterStatusView({ router }: { router: NonNullable<NetworkStatus['router']> }) {
  if (router.state === 'missing') {
    return (
      <Banner tone="warn" title="Router not deployed">
        No instance of {router.stack}/{router.service} is placed. Clients fail closed and listed hosts keep their normal route until it runs.
      </Banner>
    );
  }
  if (router.state === 'down') {
    return (
      <Banner tone="warn" title="Router is not running">
        {router.instance} is assigned to {router.node ?? 'no node'} but is not running. Clients fail closed and listed hosts keep their normal route until it runs.
      </Banner>
    );
  }
  return (
    <KV items={[
      ['Router state', <Tag key="s" tone="ok">running</Tag>],
      ['Router instance', router.instance],
      ['Router node', router.node ?? 'none'],
      ['Router address', router.ip ?? 'none'],
    ]} />
  );
}

export function NetworkDetail() {
  const { name = '' } = useParams();
  const cluster = useClusterState();
  const [config, setConfig] = useState<ClusterConfig | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<NetworkStatus | null>(null);

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

  // Live router state, only meaningful for a container network.
  const isContainer = config?.networks[name]?.type === 'container';
  useEffect(() => {
    if (!isContainer) return;
    let off = false;
    const load = () =>
      api
        .networkStatus(name)
        .then((s) => !off && setStatus(s))
        .catch(() => undefined);
    load();
    const t = setInterval(load, 10000);
    return () => {
      off = true;
      clearInterval(t);
    };
  }, [name, isContainer]);

  if (error) return <Banner tone="danger">{error}</Banner>;
  if (!config) return <p className="cl-muted">Loading network…</p>;
  const net = config.networks[name];
  if (!net) return <Banner tone="warn" title={`No network named ${name}`}>Check the name under Settings, Network.</Banner>;

  const container = net.type === 'container' ? net.container ?? null : null;
  // A container network is implemented by every node; its `hosts` list is the
  // set of nodes whose own traffic is routed through the router.
  const onNet = container
    ? (container.hosts ?? []).filter((n) => n in config.nodes).map((n) => [n, config.nodes[n]] as const)
    : Object.entries(config.nodes).filter(([, node]) => name in node.network);

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
        {container && (
          <Panel title="Router">
            <KV items={[
              ['Stack', container.stack || 'none'],
              ['Service', container.service || 'none'],
              ['Routed subnet', net.subnet || 'none'],
              ['Access', (net.access ?? []).join(', ') || 'none'],
              ['Open ports', (container.open_ports ?? []).map(describeContainerPort).join(', ') || 'none'],
              ['Reverse proxy (80/443)', container.proxy ? 'reachable' : 'blocked'],
            ]} />
            {status?.router && <RouterStatusView router={status.router} />}
          </Panel>
        )}
        <Panel title={container ? "Hosts routed through the router" : "Nodes on this network"} flush>
          <DataTable
            caption={container ? "Hosts routed through the router" : "Nodes on this network"}
            rows={onNet.map(([n, node]) => ({ id: n, name: n, address: container ? (status?.nodes[n] ? `via router (segment ${status.nodes[n]})` : 'via router') : describeAddress(node.network[name]), down: !!cluster?.unreachable.includes(n) }))}
            empty={container ? "No host is routed through the router." : "No node is configured on this network."}
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
