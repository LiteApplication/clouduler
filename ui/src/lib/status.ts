// Maps the API's own state vocabularies onto the ds StatusBadge: one icon, one
// color and one word per state, so every screen says the same thing.

import type { Status } from '../ds/StatusBadge';
import type { InstanceState, NodeState, VolumeState } from '../api/types';

export interface StatusView {
  status: Status;
  label: string;
}

export function instanceStatus(i: Pick<InstanceState, 'state' | 'health'>): StatusView {
  switch (i.state) {
    case 'running':
      if (i.health === 'unhealthy') return { status: 'degraded', label: 'Unhealthy' };
      if (i.health === 'starting') return { status: 'updating', label: 'Starting' };
      return { status: 'running', label: 'Running' };
    case 'pulling':
      return { status: 'updating', label: 'Pulling' };
    case 'restarting':
      return { status: 'updating', label: 'Restarting' };
    case 'stopping':
      return { status: 'updating', label: 'Stopping' };
    case 'exited':
      return { status: 'failed', label: 'Failed' };
    case 'pending_placement':
      return { status: 'failed', label: 'Unplaced' };
    case 'stopped':
      return { status: 'stopped', label: 'Stopped' };
    case 'unknown':
    default:
      // Another node's container: nothing to inspect it with from here, which
      // is not the same as stopped.
      return { status: 'degraded', label: 'Unknown' };
  }
}

export function nodeStatus(node: Pick<NodeState, 'availability'>, unreachable: boolean): StatusView {
  if (unreachable) return { status: 'unreachable', label: 'Unreachable' };
  if (node.availability === 'drain') return { status: 'degraded', label: 'Draining' };
  if (node.availability === 'decommissioned') return { status: 'stopped', label: 'Decommissioned' };
  return { status: 'healthy', label: 'Healthy' };
}

export function volumeStatus(v: Pick<VolumeState, 'frozen' | 'sync_refused' | 'looks_empty' | 'replicas' | 'generation'>): StatusView {
  if (v.sync_refused) return { status: 'failed', label: 'Sync refused' };
  if (v.frozen) return { status: 'stopped', label: 'Frozen' };
  if (v.looks_empty) return { status: 'degraded', label: 'Looks empty' };
  const behind = Object.values(v.replicas).some((g) => g < v.generation);
  if (behind) return { status: 'updating', label: 'Syncing' };
  return { status: 'healthy', label: 'In sync' };
}

/** A backup or restore run's state. */
export function runStatus(state: 'running' | 'ok' | 'failed' | 'pending'): StatusView {
  switch (state) {
    case 'running':
      return { status: 'updating', label: 'Running' };
    case 'ok':
      return { status: 'healthy', label: 'Succeeded' };
    case 'failed':
      return { status: 'failed', label: 'Failed' };
    default:
      return { status: 'stopped', label: 'Pending' };
  }
}

/** Rolls a stack's instance states up to one badge: any failure wins, then
 *  anything not yet settled, then all running. */
export function rollupStatus(states: Status[]): Status {
  if (states.length === 0) return 'stopped';
  if (states.includes('failed') || states.includes('unreachable')) return 'failed';
  if (states.includes('degraded')) return 'degraded';
  if (states.includes('updating')) return 'updating';
  if (states.every((s) => s === 'stopped')) return 'stopped';
  return 'running';
}
