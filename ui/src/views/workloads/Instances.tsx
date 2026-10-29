import { useMemo, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { Chip, DataTable, EmptyState, PageHeader, StatusBadge, TextField, type Column } from '../../ds';
import { instanceStatus } from '../../lib/status';
import { useInstances } from '../../state/instancesStore';
import type { InstanceState } from '../../api/types';
import './workloads.css';

type Filter = 'all' | 'running' | 'attention';

export function Instances() {
  const instances = useInstances();
  const navigate = useNavigate();
  const [filter, setFilter] = useState<Filter>('all');
  const [query, setQuery] = useState('');

  const rows = useMemo(() => {
    const q = query.trim().toLowerCase();
    return (instances ?? [])
      .filter((i) => {
        const s = instanceStatus(i).status;
        if (filter === 'running' && s !== 'running') return false;
        if (filter === 'attention' && s !== 'failed' && s !== 'degraded') return false;
        return !q || i.id.toLowerCase().includes(q) || i.node.toLowerCase().includes(q) || (i.image ?? '').toLowerCase().includes(q);
      })
      .sort((a, b) => a.id.localeCompare(b.id));
  }, [instances, filter, query]);

  if (!instances) {
    return (
      <p className="sub" role="status">
        Loading containers...
      </p>
    );
  }

  const running = instances.filter((i) => i.state === 'running').length;
  const attention = instances.filter((i) => ['failed', 'degraded'].includes(instanceStatus(i).status)).length;

  const columns: Column<InstanceState>[] = [
    { key: 'id', label: 'Container', mono: true },
    { key: 'node', label: 'Node', render: (v) => (v as string) || '-' },
    {
      key: 'state',
      label: 'Status',
      render: (_, i) => {
        const s = instanceStatus(i);
        return <StatusBadge status={s.status} label={s.label} />;
      },
    },
    {
      key: 'proxy',
      label: 'Proxy',
      render: (_, i) =>
        i.proxy ? i.proxy.error ? <StatusBadge status="failed" label="Unreachable" /> : <StatusBadge status="healthy" label="Reachable" /> : '-',
    },
    { key: 'image', label: 'Image', mono: true, render: (v) => (v as string | null) ?? '-' },
  ];

  return (
    <div className="sec wl-page">
      <PageHeader eyebrow="Workloads" title="Containers" sub={`${running} of ${instances.length} running`} />
      <div className="chips" role="group" aria-label="Filter containers">
        <Chip label="All" count={instances.length} selected={filter === 'all'} onClick={() => setFilter('all')} />
        <Chip label="Running" count={running} selected={filter === 'running'} onClick={() => setFilter('running')} />
        <Chip label="Needs attention" count={attention} selected={filter === 'attention'} onClick={() => setFilter('attention')} />
      </div>
      <TextField label="Filter by name, node or image" type="search" value={query} onChange={(e) => setQuery(e.target.value)} />
      {rows.length === 0 ? (
        <EmptyState icon="container" title={instances.length === 0 ? 'No containers yet' : 'No containers match'}>
          {instances.length === 0 ? 'Deploy a stack and its containers show up here.' : 'Try a different filter.'}
        </EmptyState>
      ) : (
        <DataTable
          caption="Containers"
          columns={columns}
          rows={rows}
          onRowClick={(i) => i.state !== 'pending_placement' && navigate(`/instances/${encodeURIComponent(i.id)}`)}
        />
      )}
    </div>
  );
}
