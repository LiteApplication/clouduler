// Pure reducer for the cluster state store. Kept apart from the hook that
// wires it to EventSource so it's trivial to unit test.

import type { ClusterState } from '../api/types';

export type ConnectionStatus = 'connecting' | 'open' | 'closed';

export interface StoreState {
  cluster: ClusterState | null;
  status: ConnectionStatus;
  error: string | null;
}

export type StoreAction =
  | { type: 'connecting' }
  | { type: 'open' }
  | { type: 'state'; cluster: ClusterState }
  | { type: 'error'; message: string }
  | { type: 'closed' };

export const initialStoreState: StoreState = {
  cluster: null,
  status: 'connecting',
  error: null,
};

export function reducer(state: StoreState, action: StoreAction): StoreState {
  switch (action.type) {
    case 'connecting':
      return { ...state, status: 'connecting' };
    case 'open':
      return { ...state, status: 'open', error: null };
    case 'state':
      // A stale event (lower version than what we already have) is ignored.
      // /api/events can redeliver on reconnect, and we never want to render
      // an older snapshot over a newer one.
      if (state.cluster && action.cluster.version < state.cluster.version) {
        return state;
      }
      return { ...state, cluster: action.cluster, status: 'open', error: null };
    case 'error':
      return { ...state, error: action.message };
    case 'closed':
      return { ...state, status: 'closed' };
    default:
      return state;
  }
}
