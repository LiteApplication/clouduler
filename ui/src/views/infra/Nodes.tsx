// /nodes: cluster health at the top, every node in one table, the "who can
// reach whom" matrix, and the admin actions (rebalance, add a node).

import { useEffect, useMemo, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { api, ApiError } from '../../api/client';
import type { ClusterStatus, Connectivity, JoinCommand, LastRebalance } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Banner, BottomSheet, Button, Chip, EmptyState, Meter, NodeCard, PageHeader, Panel, StatusBadge, Tag, TextField, useConfirm, useToast } from '../../ds';
import { useNodeMetrics, cpuPercent, memPercent } from '../../hooks/useNodeMetrics';
import { formatUptime, relativeTime } from '../../lib/format';
import { cellState, type CellState } from '../../lib/links';
import { nodeStatus } from '../../lib/status';
import { useInstances } from '../../state/instancesStore';
import { useClusterState } from '../../state/store';
import { useConfigDraft } from '../config/draft';
import { DataTable } from './table';
import './infra.css';

interface Row {
  id: string;
  name: string;
  role: string;
  availability: string;
  unreachable: boolean;
  pending: boolean;
  address: string;
}

/** Text first, colour second: every cell says what it is in words. */
function CellTag({ state }: { state: CellState }) {
  switch (state.kind) {
    case 'self':
      return <span className="cl-muted">self</span>;
    case 'ok':
      return <Tag tone="ok">{state.ms != null ? `${state.ms} ms` : 'ok'}</Tag>;
    case 'down':
      return <Tag tone="danger">Unreachable</Tag>;
    case 'api-down':
      return <Tag tone="warn">API down</Tag>;
    case 'no-report':
      return <Tag tone="warn">No report</Tag>;
  }
}

export function Nodes() {
  const cluster = useClusterState();
  const instances = useInstances();
  const { original: config } = useConfigDraft();
  const { isAdmin } = useAuth();
  const navigate = useNavigate();
  const confirm = useConfirm();
  const toast = useToast();
  const [status, setStatus] = useState<ClusterStatus | null>(null);
  const [links, setLinks] = useState<Connectivity | null>(null);
  const [last, setLast] = useState<LastRebalance | null>(null);
  const [rebalancing, setRebalancing] = useState(false);
  const [adding, setAdding] = useState(false);

  const names = useMemo(() => Object.keys(cluster?.nodes ?? {}), [cluster]);
  const { metrics } = useNodeMetrics(names);

  useEffect(() => {
    let off = false;
    const load = () => {
      api.status().then((s) => !off && setStatus(s)).catch(() => {});
      api.nodeConnectivity().then((c) => !off && setLinks(c)).catch(() => {});
    };
    load();
    const id = setInterval(load, 10000);
    api.lastRebalance().then((r) => !off && setLast(r)).catch(() => {});
    return () => {
      off = true;
      clearInterval(id);
    };
  }, []);

  // A node the config declares but the live roster hasn't admitted yet (still
  // joining, or down) would be indistinguishable from a name nobody configured.
  // Listing it as pending is what tells an operator "this name is taken".
  const pendingNames = useMemo(
    () => Object.keys(config?.nodes ?? {}).filter((n) => !cluster?.nodes[n]),
    [config, cluster],
  );

  if (!cluster) return <p className="cl-muted">Loading nodes…</p>;

  const unreachable = cluster.unreachable;
  const rows: Row[] = [
    ...Object.values(cluster.nodes).map((n) => ({
      id: n.name,
      name: n.name,
      role: n.role,
      availability: n.availability,
      unreachable: unreachable.includes(n.name),
      pending: false,
      address: n.rpc_addr,
    })),
    ...pendingNames.map((n) => ({
      id: n,
      name: n,
      role: config?.nodes[n]?.role ?? '-',
      availability: 'pending',
      unreachable: true,
      pending: true,
      address: '-',
    })),
  ];
  const containersOn = (node: string) => (instances ? instances.filter((i) => i.node === node && i.running).length : null);
  const raftRole = (name: string) => (status?.leader === name ? ('leader' as const) : status ? ('follower' as const) : undefined);

  const rebalance = async () => {
    const ok = await confirm({
      title: 'Rebalance the cluster?',
      body: 'Instances may move between nodes to even out load. Each move restarts that instance.',
      confirmLabel: 'Rebalance cluster',
      confirmVariant: 'primary',
    });
    if (!ok) return;
    setRebalancing(true);
    try {
      const result = await api.rebalance();
      toast(result.moves.length === 0 ? 'Already balanced. Nothing to move.' : `Moving ${result.moves.length} instance${result.moves.length === 1 ? '' : 's'}.`);
      api.lastRebalance().then(setLast).catch(() => {});
    } catch (e) {
      toast(e instanceof ApiError ? e.message : 'Could not rebalance.', { tone: 'danger' });
    } finally {
      setRebalancing(false);
    }
  };

  const sorted = rows.filter((r) => !r.pending).map((r) => r.name).sort();

  return (
    <>
      <PageHeader
        eyebrow="Cluster"
        title="Nodes"
        sub={`${rows.length} node${rows.length === 1 ? '' : 's'}${unreachable.length ? ` · ${unreachable.length} unreachable` : ''}${pendingNames.length ? ` · ${pendingNames.length} pending` : ''}`}
        actions={
          isAdmin && (
            <>
              <Button icon="sync" loading={rebalancing} onClick={() => void rebalance()}>Rebalance cluster</Button>
              <Button variant="primary" icon="plus" onClick={() => setAdding(true)}>Add node</Button>
            </>
          )
        }
      />

      <div className="infra-stack">
        {status && !status.has_quorum && (
          <Banner tone="danger" title="No quorum">
            This node can reach {status.reachable} of {status.voters} voters, so changes are paused. Bring a manager back online.
            {cluster.emergency_stack && <> Emergency mode is on for <code>{cluster.emergency_stack}</code>. It clears when the managers reconnect.</>}
          </Banner>
        )}
        {unreachable.length > 0 && status?.has_quorum && (
          <Banner tone="warn" title={`${unreachable.join(', ')} ${unreachable.length === 1 ? 'is' : 'are'} unreachable`}>
            Its containers move elsewhere once the failover timeout passes.
          </Banner>
        )}

        {rows.length === 0 ? (
          <EmptyState icon="node" title="No nodes yet">Add your first node to get started.</EmptyState>
        ) : (
          <div className="grid">
            {rows.filter((r) => !r.pending).map((r) => {
              const u = metrics[r.name];
              const v = nodeStatus({ availability: cluster.nodes[r.name].availability }, r.unreachable);
              return (
                <NodeCard
                  key={r.name}
                  name={r.name}
                  address={r.address}
                  status={v.status}
                  role={raftRole(r.name)}
                  cpu={u ? cpuPercent(u) : null}
                  mem={u ? memPercent(u) : null}
                  containers={containersOn(r.name)}
                  onClick={() => navigate(`/nodes/${encodeURIComponent(r.name)}`)}
                />
              );
            })}
          </div>
        )}

        <Panel title="All nodes" flush>
          <DataTable
            caption="All nodes"
            rows={rows}
            empty="No nodes yet."
            onRowClick={(r) => navigate(`/nodes/${encodeURIComponent(r.name)}`)}
            columns={[
              { key: 'name', header: 'Node', render: (r) => <strong>{r.name}</strong> },
              {
                key: 'role',
                header: 'Role',
                render: (r) => (
                  <>
                    {r.role} {status?.leader === r.name && <Tag tone="pop">leader</Tag>}
                  </>
                ),
              },
              {
                key: 'state',
                header: 'State',
                render: (r) => {
                  if (r.pending) return <Tag>pending</Tag>;
                  const v = nodeStatus({ availability: cluster.nodes[r.name].availability }, r.unreachable);
                  return <StatusBadge status={v.status} label={v.label} />;
                },
              },
              { key: 'cpu', header: 'CPU', render: (r) => pct(metrics[r.name] && cpuPercent(metrics[r.name])) },
              { key: 'mem', header: 'Memory', render: (r) => pct(metrics[r.name] && memPercent(metrics[r.name])) },
              { key: 'cont', header: 'Containers', render: (r) => containersOn(r.name) ?? '-' },
              { key: 'up', header: 'Uptime', render: (r) => (metrics[r.name]?.uptime_seconds ? formatUptime(metrics[r.name].uptime_seconds) : '-') },
            ]}
          />
        </Panel>

        {sorted.length > 0 && (
          <Panel title="Who can reach whom" flush>
            <p className="cl-sm cl-muted" style={{ padding: '0 var(--space-4)' }}>
              Raft and API links, round trip in milliseconds. Rows report, columns are peers.
            </p>
            {sorted.length === 1 && (
              <p className="cl-sm cl-muted" style={{ padding: '0 var(--space-4)' }}>
                Links appear here once there is more than one node.
              </p>
            )}
            <div className="cl-tablewrap">
              <table className="cl-table">
                <caption className="cl-sr">Node connectivity</caption>
                <thead>
                  <tr>
                    <th scope="col">From</th>
                    {sorted.map((n) => <th scope="col" key={n}>{n}</th>)}
                  </tr>
                </thead>
                <tbody>
                  {sorted.map((from) => (
                    <tr key={from}>
                      <th scope="row">{from}</th>
                      {sorted.map((to) => (
                        <td key={to} className="cl-td--mono" data-label={to}>
                          <CellTag state={cellState(links, from, to)} />
                        </td>
                      ))}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </Panel>
        )}

        {(last || cluster.removed.length > 0) && (
          <Panel title="History">
            {last && (
              <p>
                Last rebalance <strong>{relativeTime(last.at)}</strong>:{' '}
                {last.moves.length === 0 ? 'nothing needed to move.' : `${last.moves.length} move${last.moves.length === 1 ? '' : 's'}.`}
              </p>
            )}
            {last?.moves.map((m) => (
              <div key={m.instance} className="li"><code>{m.instance}</code><span className="cl-muted">{m.from} → {m.to}</span></div>
            ))}
            {cluster.removed.length > 0 && (
              <p className="cl-sm cl-muted">
                Removed names, held until reused: {cluster.removed.map((n) => <code key={n} style={{ marginRight: 8 }}>{n}</code>)}
              </p>
            )}
          </Panel>
        )}
      </div>

      <AddNodeSheet open={adding} onClose={() => setAdding(false)} />
    </>
  );
}

function pct(v: number | null | undefined) {
  return v == null ? '-' : <Meter label="" value={v} />;
}

function AddNodeSheet({ open, onClose }: { open: boolean; onClose: () => void }) {
  const toast = useToast();
  const [name, setName] = useState('');
  const [role, setRole] = useState<'manager' | 'worker'>('worker');
  const [address, setAddress] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [join, setJoin] = useState<JoinCommand | null>(null);

  const close = () => {
    setJoin(null);
    setName('');
    setAddress('');
    setError(null);
    onClose();
  };
  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      setJoin(await api.createJoinCommand({ name: name.trim(), role, address: address.trim() || undefined }));
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Could not create the join command.');
    } finally {
      setBusy(false);
    }
  };
  const copy = async () => {
    if (!join) return;
    try {
      await navigator.clipboard.writeText(join.command);
      toast('Copied the join command.');
    } catch {
      toast('Could not copy. Select the command instead.', { tone: 'warn' });
    }
  };

  return (
    <BottomSheet
      open={open}
      title={join ? 'Run this on the new node' : 'Add a node'}
      onClose={close}
      footer={
        join ? (
          <>
            <Button icon="copy" onClick={() => void copy()}>Copy command</Button>
            <Button variant="primary" onClick={close}>Done</Button>
          </>
        ) : (
          <Button variant="primary" icon="plus" loading={busy} disabled={!name.trim()} onClick={() => void submit()}>
            Create join command
          </Button>
        )
      }
    >
      {join ? (
        <>
          <p className="cl-sm cl-muted">
            It carries a one-time API key and is shown once. If you don't use it, delete the key{' '}
            <code>{join.key_id}</code> under Settings, Access.
          </p>
          <pre className="codeblock" style={{ whiteSpace: 'pre-wrap', wordBreak: 'break-all' }}>{join.command}</pre>
        </>
      ) : (
        <>
          <TextField label="Node name" mono value={name} onChange={(e) => setName(e.target.value)} />
          <div className="chips" role="group" aria-label="Role">
            <Chip label="Worker" selected={role === 'worker'} onClick={() => setRole('worker')} />
            <Chip label="Manager" selected={role === 'manager'} onClick={() => setRole('manager')} />
          </div>
          <TextField label="Address" mono value={address} onChange={(e) => setAddress(e.target.value)} hint="Optional. How the other nodes reach this one." error={error ?? undefined} />
        </>
      )}
    </BottomSheet>
  );
}
