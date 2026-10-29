// Per-stack rollups shared by the Overview and the Stacks list: one status, one
// set of counts and a "what is happening" line, derived from the live instance
// list plus the replicated update records.

import type { ClusterState, InstanceState } from '../../api/types';
import type { Status } from '../../ds';
import { instanceStatus, rollupStatus } from '../../lib/status';
import { isUpdateActive, UPDATE_PHASE_LABEL } from '../../lib/updates';
import { relativeTime } from '../../lib/format';

export interface StackSummary {
  name: string;
  services: number;
  nodes: number;
  status: Status;
  counts: Partial<Record<'running' | 'degraded' | 'failed' | 'stopped', number>>;
  /** One short line under the card: the worst problem, an update in flight, or the deploy age. */
  note: string;
}

export function stackSummaries(cluster: ClusterState, instances: InstanceState[] | null): StackSummary[] {
  const live = new Map((instances ?? []).map((i) => [i.id, i]));
  return Object.values(cluster.stacks)
    .map((stack): StackSummary => {
      const mine = Object.values(cluster.assignments).filter((a) => a.instance.split('.')[0] === stack.name);
      const statuses: Status[] = [];
      const counts: StackSummary['counts'] = {};
      let note = '';
      for (const a of mine) {
        const inst = live.get(a.instance);
        const update = cluster.instance_updates[a.instance];
        let s: Status;
        if (isUpdateActive(update)) {
          s = 'updating';
          note ||= `Updating ${a.instance.split('.')[1]}: ${UPDATE_PHASE_LABEL[update.phase]}`;
        } else if (inst) {
          s = instanceStatus(inst).status;
          if (s === 'failed' && !note) note = `${a.instance.split('.')[1]} failed on ${a.node}`;
        } else {
          s = a.desired === 'stopped' ? 'stopped' : 'running';
        }
        statuses.push(s);
        if (s === 'running' || s === 'degraded' || s === 'failed' || s === 'stopped') counts[s] = (counts[s] ?? 0) + 1;
        else if (s === 'updating') counts.running = (counts.running ?? 0) + 1;
      }
      if (!note) note = stack.deployed_at ? `Deployed ${relativeTime(stack.deployed_at)}` : `Revision ${stack.revision}`;
      return {
        name: stack.name,
        services: new Set(mine.map((a) => a.instance.split('.')[1])).size || countServices(stack.spec),
        nodes: new Set(mine.map((a) => a.node)).size,
        status: rollupStatus(statuses),
        counts,
        note,
      };
    })
    .sort(byUrgency);
}

const ORDER: Record<string, number> = { failed: 0, unreachable: 0, degraded: 1, updating: 2, running: 3, healthy: 3, stopped: 4 };

/** Failed stacks first, then by name. */
function byUrgency(a: StackSummary, b: StackSummary): number {
  return ORDER[a.status] - ORDER[b.status] || a.name.localeCompare(b.name);
}

export const STACK_FILTERS: { id: 'all' | 'running' | 'updating' | 'failed'; label: string }[] = [
  { id: 'all', label: 'All' },
  { id: 'running', label: 'Running' },
  { id: 'updating', label: 'Updating' },
  { id: 'failed', label: 'Failed' },
];

export function matchesFilter(s: StackSummary, filter: string): boolean {
  if (filter === 'all') return true;
  if (filter === 'failed') return s.status === 'failed' || s.status === 'degraded';
  return s.status === filter;
}

// Cheap service count without a full yaml parser: count top level keys under
// `services:`. Only a fallback for a stack with no assignments yet.
export function countServices(spec: string): number {
  const lines = spec.split('\n');
  const idx = lines.findIndex((l) => /^services:\s*$/.test(l));
  if (idx === -1) return 0;
  let count = 0;
  for (let i = idx + 1; i < lines.length; i++) {
    const line = lines[i];
    if (/^\S/.test(line)) break;
    if (/^  \S[^:]*:\s*$/.test(line) || /^  \S[^:]*:\s*\{/.test(line)) count++;
  }
  return count;
}
