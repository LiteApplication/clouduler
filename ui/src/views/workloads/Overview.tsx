// Cluster overview: banners for what needs a person, the Raft/quorum hero with
// aggregate meters, three at-a-glance panels, then the node and stack grids.

import { useEffect, useMemo, useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { api } from '../../api/client';
import type { ClusterStatus, UpdateRow } from '../../api/types';
import { useClusterState } from '../../state/store';
import { useInstances } from '../../state/instancesStore';
import { cpuPercent, memPercent, useNodeMetrics } from '../../hooks/useNodeMetrics';
import { Banner, Button, Chip, Icon, Meter, NodeCard, PageHeader, StackCard, StatusBadge, type Status } from '../../ds';
import { daysUntil, formatBytes, relativeTime } from '../../lib/format';
import { nodeStatus } from '../../lib/status';
import { matchesFilter, STACK_FILTERS, stackSummaries } from './stackStatus';

const CERT_WARN_DAYS = 14;

/** "v0.14.2 on all nodes", or the mix when nodes disagree. */
function binarySummary(versions: (string | undefined)[], total: number, fallback?: string): string {
  const known = versions.filter((v): v is string => !!v);
  const uniq = [...new Set(known)];
  if (uniq.length === 0) return fallback ? `v${fallback}` : '-';
  if (uniq.length === 1 && known.length === total) return `v${uniq[0]} on all nodes`;
  return uniq.map((v) => `v${v} on ${known.filter((k) => k === v).length}`).join(', ');
}

export function Overview() {
  const cluster = useClusterState();
  const navigate = useNavigate();
  const instances = useInstances();
  const [status, setStatus] = useState<ClusterStatus | null>(null);
  const [updates, setUpdates] = useState<UpdateRow[] | null>(null);
  const [filter, setFilter] = useState<string>('all');
  const nodeNames = useMemo(() => Object.keys(cluster?.nodes ?? {}), [cluster?.nodes]);
  const { metrics } = useNodeMetrics(nodeNames);

  useEffect(() => {
    let cancelled = false;
    const load = () => api.status().then((s) => !cancelled && setStatus(s)).catch(() => {});
    void load();
    // Version bumps arrive over the event stream; polling status here is just
    // a cheap way to keep quorum and leadership fresh, which the state event
    // doesn't carry.
    const id = setInterval(load, 10_000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    // The registry check is slow and optional: the panel just stays quiet if it fails.
    Promise.resolve(api.listUpdates?.())
      .then((rows) => !cancelled && setUpdates(rows ?? null))
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  const summaries = useMemo(() => (cluster ? stackSummaries(cluster, instances) : []), [cluster, instances]);

  if (!cluster) {
    return (
      <p className="pageload">
        <span className="spinner" /> Connecting to cluster...
      </p>
    );
  }

  const nodes = Object.values(cluster.nodes).sort((a, b) => a.name.localeCompare(b.name));
  const unreachable = new Set(cluster.unreachable);
  const hasQuorumProblem = status ? !status.has_quorum : false;
  const volumes = Object.values(cluster.volumes);

  const failedInstances = (instances ?? []).filter((i) => i.state === 'exited' || i.state === 'pending_placement');
  const soonCerts = Object.values(cluster.certificates)
    .map((c) => ({ fqdn: c.fqdn, days: daysUntil(c.not_after) }))
    .filter((c) => c.days <= CERT_WARN_DAYS)
    .sort((a, b) => a.days - b.days);
  const nextCert = Object.values(cluster.certificates)
    .map((c) => ({ fqdn: c.fqdn, days: daysUntil(c.not_after) }))
    .sort((a, b) => a.days - b.days)[0];

  const managers = nodes.filter((n) => n.role === 'manager');
  const workers = nodes.filter((n) => n.role === 'worker');
  const reachable = (list: typeof nodes) => list.filter((n) => !unreachable.has(n.name)).length;

  // Aggregate CPU and memory over the nodes that answered.
  const usages = Object.values(metrics);
  const cpus = usages.map(cpuPercent).filter((v): v is number => v != null);
  const clusterCpu = cpus.length ? cpus.reduce((a, b) => a + b, 0) / cpus.length : null;
  const memTotal = usages.reduce((a, u) => a + u.memory_total_bytes, 0);
  const memUsed = usages.reduce((a, u) => a + u.memory_used_bytes, 0);

  const running = (instances ?? []).filter((i) => i.state === 'running').length;
  const total = instances ? instances.length : Object.keys(cluster.assignments).length;

  // Updates panel: images with something newer, plus updates that failed.
  const waiting = (updates ?? []).filter((u) => u.update_available).length;
  const failedUpdates = Object.values(cluster.instance_updates).filter((u) => u.phase === 'failed').length;
  const pulling = Object.entries(cluster.instance_updates).find(([, u]) => u.phase !== 'failed' && u.phase !== 'started');

  // Backups panel: the newest run and how many enabled targets ran within two periods.
  const targets = Object.values(cluster.restic_targets).filter((t) => t.enabled);
  const runs = Object.values(cluster.restic_runs).sort((a, b) => b.started_at - a.started_at);
  const lastRun = runs[0];
  const nowSecs = Math.floor(Date.now() / 1000);
  const onSchedule = targets.filter((t) => t.last_run != null && nowSecs - t.last_run <= t.frequency_secs * 2).length;
  const lastRunLabel = !lastRun
    ? null
    : 'Failed' in lastRun.outcome
      ? 'failed'
      : 'Running' in lastRun.outcome
        ? 'running'
        : 'succeeded';

  const shown = summaries.filter((s) => matchesFilter(s, filter));

  return (
    <>
      <PageHeader
        eyebrow={status?.node ? `Answered by ${status.node}` : 'Cluster'}
        title="Overview"
        actions={
          <>
            <Button variant="primary" icon="plus" onClick={() => navigate('/stacks/new')}>
              Deploy stack
            </Button>
          </>
        }
      />

      <div className="col">
        {hasQuorumProblem && (
          <Banner tone="danger" title="Quorum lost">
            The cluster cannot accept writes until enough managers are reachable.
          </Banner>
        )}
        {unreachable.size > 0 && (
          <Banner tone="danger" title={`${unreachable.size} node${unreachable.size > 1 ? 's' : ''} unreachable`} action="View nodes" onAction={() => navigate('/nodes')}>
            {[...unreachable].join(', ')}
          </Banner>
        )}
        {failedInstances.slice(0, 3).map((i) => (
          <Banner
            key={i.id}
            tone="danger"
            title={`${i.id.split('.')[1]} failed on ${i.node}`}
            action="View logs"
            onAction={() => navigate(`/instances/${encodeURIComponent(i.id)}`)}
          >
            {i.state === 'pending_placement' ? (i.placement_error ?? 'No node can run it yet.') : 'Restart it or view logs.'}
          </Banner>
        ))}
        {failedInstances.length > 3 && <Banner tone="danger" title={`${failedInstances.length - 3} more failed instances`} action="All instances" onAction={() => navigate('/instances')} />}
        {soonCerts.slice(0, 2).map((c) => (
          <Banner
            key={c.fqdn}
            tone="warn"
            title={c.days <= 0 ? `${c.fqdn} has expired` : `${c.fqdn} expires in ${c.days} day${c.days === 1 ? '' : 's'}`}
            action="Open DNS"
            onAction={() => navigate('/dns')}
          />
        ))}
        {volumes.filter((v) => v.sync_refused).slice(0, 2).map((v) => (
          <Banner key={`refused-${v.id}`} tone="danger" title={`Sync refused for ${v.id}`} action="Open storage" onAction={() => navigate('/storage')}>
            {v.sync_refused!.reason}
          </Banner>
        ))}
        {volumes.filter((v) => v.looks_empty).slice(0, 2).map((v) => (
          <Banner key={`empty-${v.id}`} tone="danger" title={`${v.id} looks empty`} action="Open storage" onAction={() => navigate('/storage')}>
            Its source found no files on two scans in a row. Check it before the empty copy replicates.
          </Banner>
        ))}
      </div>

      <section className="hero" aria-labelledby="cluster-h">
        <div className="col" style={{ flex: '1 1 320px', minWidth: 0 }}>
          <span className="eyebrow" id="cluster-h">
            Cluster health
          </span>
          <div className="row">
            {status?.leader && (
              <span className="cl-role cl-role--leader">
                <Icon name="crown" size={14} />
                <span>Raft leader {status.leader}</span>
              </span>
            )}
            {status && (
              <StatusBadge
                status={status.has_quorum ? 'healthy' : 'failed'}
                label={status.has_quorum ? `Quorum ${status.managers_reachable} of ${status.managers_total}` : 'No quorum'}
              />
            )}
          </div>
          {status && status.has_quorum && unreachable.size === 0 && (
            <p className="sub">
              Cluster healthy, quorum held. All {nodes.length} node{nodes.length === 1 ? '' : 's'} purring.
            </p>
          )}
          <dl className="kv" style={{ marginTop: 4 }}>
            <dt>Managers</dt>
            <dd>
              {reachable(managers)} of {managers.length} reachable
            </dd>
            <dt>Workers</dt>
            <dd>
              {reachable(workers)} of {workers.length} reachable
            </dd>
            <dt>Binary</dt>
            <dd>{binarySummary(nodes.map((n) => n.version ?? metrics[n.name]?.version), nodes.length, status?.version)}</dd>
            <dt>State</dt>
            <dd>version {status?.state_version ?? cluster.version}</dd>
          </dl>
        </div>
        <div className="col" style={{ flex: '1 1 320px', minWidth: 0, justifyContent: 'center' }}>
          <Meter
            label="Containers"
            value={running}
            max={Math.max(total, 1)}
            display={`${running} of ${total} running`}
            tone="ok"
          />
          {clusterCpu != null ? <Meter label="Cluster CPU" value={clusterCpu} /> : <Meter label="Cluster CPU" value={0} display="-" />}
          {memTotal > 0 ? (
            <Meter
              label="Cluster memory"
              value={memUsed}
              max={memTotal}
              display={`${formatBytes(memUsed)} / ${formatBytes(memTotal)}`}
            />
          ) : (
            <Meter label="Cluster memory" value={0} display="-" />
          )}
        </div>
      </section>

      <section className="grid grid--3" aria-label="At a glance">
        <Link className="panel" to="/updates">
          <span className="row eyebrow">
            <Icon name="sync" size={16} />
            Image updates
          </span>
          <span className="metric">{updates ? waiting + failedUpdates : failedUpdates || '-'}</span>
          <span className="sub">
            {updates
              ? waiting + failedUpdates === 0
                ? 'Everything is up to date.'
                : `${waiting} waiting, ${failedUpdates} failed.`
              : failedUpdates > 0
                ? `${failedUpdates} failed.`
                : 'Checking registries...'}
            {pulling && ` ${pulling[0]} is updating now.`}
          </span>
        </Link>
        <Link className="panel" to="/storage/backups">
          <span className="row eyebrow">
            <Icon name="archive" size={16} />
            Backups
          </span>
          <span className="metric">{lastRun ? relativeTime(lastRun.started_at) : 'None yet'}</span>
          <span className="sub">
            {lastRun ? `Last backup ${lastRunLabel}. ` : ''}
            {targets.length > 0 ? `${onSchedule} of ${targets.length} targets on schedule.` : 'No backup targets scheduled.'}
          </span>
        </Link>
        <Link className="panel" to="/dns">
          <span className="row eyebrow">
            <Icon name="globe" size={16} />
            Certificates
          </span>
          <span className="metric">{nextCert ? `${Math.max(nextCert.days, 0)} days` : '-'}</span>
          <span className="sub">
            {nextCert ? (
              <>
                until the next one expires: <span className="mono">{nextCert.fqdn}</span>
              </>
            ) : (
              'No certificates issued yet.'
            )}
          </span>
        </Link>
      </section>

      <section className="sec" aria-labelledby="nodes-h">
        <div className="sec__h">
          <h2 className="h2" id="nodes-h">
            Nodes
          </h2>
          <Link className="lnk" to="/nodes">
            All nodes
          </Link>
        </div>
        {nodes.length === 0 ? (
          <p className="sub">No nodes yet.</p>
        ) : (
          <div className="grid">
            {nodes.map((n) => {
              const u = metrics[n.name];
              const disk = u?.drives?.length
                ? Math.max(...u.drives.map((d) => (d.total_bytes > 0 ? (1 - d.free_bytes / d.total_bytes) * 100 : 0)))
                : null;
              const containers = Object.values(cluster.assignments).filter((a) => a.node === n.name).length;
              return (
                <NodeCard
                  key={n.name}
                  name={n.name}
                  address={n.rpc_addr.split(':')[0]}
                  status={nodeStatus(n, unreachable.has(n.name)).status}
                  role={status?.leader === n.name ? 'leader' : n.role === 'manager' ? 'follower' : undefined}
                  cpu={u ? cpuPercent(u) : null}
                  mem={u ? memPercent(u) : null}
                  disk={disk}
                  containers={containers}
                  onClick={() => navigate(`/nodes/${encodeURIComponent(n.name)}`)}
                />
              );
            })}
          </div>
        )}
      </section>

      <section className="sec" aria-labelledby="stacks-h">
        <div className="sec__h">
          <h2 className="h2" id="stacks-h">
            Stacks
          </h2>
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
        </div>
        {summaries.length === 0 ? (
          <p className="sub">No stacks yet. Deploy your first one.</p>
        ) : (
          <div className="grid">
            {shown.map((s) => (
              <StackCard
                key={s.name}
                name={s.name}
                services={s.services}
                nodes={s.nodes}
                status={s.status as Status}
                counts={s.counts}
                updated={s.note}
                onClick={() => navigate(`/stacks/${encodeURIComponent(s.name)}`)}
              />
            ))}
          </div>
        )}
      </section>
    </>
  );
}
