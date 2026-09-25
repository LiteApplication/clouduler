// Shared reading of `ClusterState.instance_updates`: which steps an update
// goes through, and how each one is shown.

import type { InstanceUpdate, UpdatePhase } from '../api/types';

/** The four steps with a segment in the progress bar. `queued` comes before
 *  all of them, `failed` can replace any of them. */
export const UPDATE_STEPS = ['pulling', 'stopping', 'starting', 'started'] as const;
export type UpdateStep = (typeof UPDATE_STEPS)[number];

export const UPDATE_PHASE_LABEL: Record<UpdatePhase, string> = {
  queued: 'queued',
  pulling: 'downloading image',
  stopping: 'stopping old container',
  starting: 'starting new container',
  started: 'started',
  failed: 'failed',
};

export function isUpdateActive(update: InstanceUpdate | undefined): update is InstanceUpdate {
  return !!update && update.phase !== 'started' && update.phase !== 'failed';
}

/** The same `reason`/`step` pair an instance summary carries, straight from
 *  the replicated record: `null` once the update has started its new
 *  container, since from there on the instance is steady-state again. */
export function updateProgress(update: InstanceUpdate): { reason: string; step: [number, number] | null } | null {
  switch (update.phase) {
    case 'started':
      return null;
    case 'queued':
      return { reason: 'update queued', step: null };
    case 'failed':
      return { reason: `update failed: ${update.error ?? 'unknown error'}`, step: null };
    default: {
      const step = UPDATE_STEPS.indexOf(update.phase) + 1;
      return {
        reason: `updating (${UPDATE_PHASE_LABEL[update.phase]}, step ${step}/${UPDATE_STEPS.length})`,
        step: [step, UPDATE_STEPS.length],
      };
    }
  }
}
