// Pure reducer for the instance progress store. Kept apart from the hook
// that wires it to EventSource so it's trivial to unit test — mirrors
// state/reducer.ts, the cluster state store's own reducer.

import type { InstanceProgressEvent } from '../api/types';

export interface InstanceProgress {
  reason: string | null;
  step: [number, number] | null;
}

export interface ProgressState {
  byId: Record<string, InstanceProgress>;
}

export type ProgressAction = { type: 'progress'; event: InstanceProgressEvent } | { type: 'reset' };

export const initialProgressState: ProgressState = { byId: {} };

export function progressReducer(state: ProgressState, action: ProgressAction): ProgressState {
  switch (action.type) {
    case 'progress':
      return {
        byId: {
          ...state.byId,
          [action.event.instance_id]: { reason: action.event.reason, step: action.event.step },
        },
      };
    case 'reset':
      return initialProgressState;
    default:
      return state;
  }
}
