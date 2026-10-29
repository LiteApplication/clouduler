import { useMemo, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { useClusterState } from '../../state/store';
import { useInstances } from '../../state/instancesStore';
import { useAuth } from '../../auth/AuthContext';
import { Button, Chip, EmptyState, PageHeader, StackCard } from '../../ds';
import { matchesFilter, STACK_FILTERS, stackSummaries } from './stackStatus';

export function Stacks() {
  const cluster = useClusterState();
  const instances = useInstances();
  const { isAdmin } = useAuth();
  const navigate = useNavigate();
  const [filter, setFilter] = useState('all');
  const summaries = useMemo(() => (cluster ? stackSummaries(cluster, instances) : []), [cluster, instances]);

  if (!cluster) {
    return (
      <p className="pageload">
        <span className="spinner" /> Loading stacks...
      </p>
    );
  }
  const shown = summaries.filter((s) => matchesFilter(s, filter));

  return (
    <>
      <PageHeader
        eyebrow="Workloads"
        title="Stacks"
        sub={`${summaries.length} deployed stack${summaries.length === 1 ? '' : 's'}`}
        actions={
          isAdmin && (
            <Button variant="primary" icon="plus" onClick={() => navigate('/stacks/new')}>
              Deploy stack
            </Button>
          )
        }
      />
      {summaries.length === 0 ? (
        <EmptyState
          icon="stack"
          title="No stacks yet"
          action={
            isAdmin && (
              <Button variant="primary" icon="plus" onClick={() => navigate('/stacks/new')}>
                Deploy stack
              </Button>
            )
          }
        >
          Deploy your first one.
        </EmptyState>
      ) : (
        <section className="sec" aria-label="Stacks">
          <div className="chips">
            {STACK_FILTERS.map((f) => (
              <Chip
                key={f.id}
                label={f.label}
                count={summaries.filter((s) => matchesFilter(s, f.id)).length}
                selected={filter === f.id}
                onClick={() => setFilter(f.id)}
              />
            ))}
          </div>
          <div className="grid">
            {shown.map((s) => (
              <StackCard
                key={s.name}
                name={s.name}
                services={s.services}
                nodes={s.nodes}
                status={s.status}
                counts={s.counts}
                updated={s.note}
                onClick={() => navigate(`/stacks/${encodeURIComponent(s.name)}`)}
              />
            ))}
          </div>
        </section>
      )}
    </>
  );
}
