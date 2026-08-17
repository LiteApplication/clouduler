// One EventSource subscription for the whole app, so no view polls on its
// own. /api/events emits `state` events carrying the full ClusterState body
// whenever the version moves; we just drop that straight into the reducer.

import { createContext, useContext, useEffect, useReducer, type ReactNode } from 'react';
import type { ClusterState } from '../api/types';
import { initialStoreState, reducer, type StoreState } from './reducer';

const ClusterStoreContext = createContext<StoreState | null>(null);

export function ClusterStoreProvider({ children }: { children: ReactNode }) {
  const [state, dispatch] = useReducer(reducer, initialStoreState);

  useEffect(() => {
    dispatch({ type: 'connecting' });
    const source = new EventSource('/api/events');

    source.addEventListener('open', () => dispatch({ type: 'open' }));
    source.addEventListener('state', (evt) => {
      try {
        const cluster = JSON.parse((evt as MessageEvent).data) as ClusterState;
        dispatch({ type: 'state', cluster });
      } catch {
        dispatch({ type: 'error', message: 'received a malformed state event' });
      }
    });
    source.addEventListener('error', () => {
      // EventSource retries on its own; just surface that we're not live.
      dispatch({ type: 'error', message: 'lost connection to the cluster event stream' });
    });

    return () => {
      source.close();
      dispatch({ type: 'closed' });
    };
  }, []);

  return <ClusterStoreContext.Provider value={state}>{children}</ClusterStoreContext.Provider>;
}

export function useClusterStore(): StoreState {
  const ctx = useContext(ClusterStoreContext);
  if (!ctx) throw new Error('useClusterStore must be used inside ClusterStoreProvider');
  return ctx;
}

/** Convenience for views that only care about the state once it has arrived. */
export function useClusterState(): ClusterState | null {
  return useClusterStore().cluster;
}
