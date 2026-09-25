// One EventSource subscription for the whole app for instance progress,
// mirroring state/store.tsx's ClusterStoreProvider. /api/instances/events
// emits a `progress` event per instance whenever its (state, reason, step)
// changes; we just drop that into the reducer.

import { createContext, useContext, useEffect, useReducer, type ReactNode } from 'react';
import type { InstanceProgressEvent } from '../api/types';
import { updateProgress } from '../lib/updates';
import { useClusterState } from './store';
import { initialProgressState, progressReducer, type InstanceProgress, type ProgressState } from './progressReducer';

const InstanceProgressContext = createContext<ProgressState | null>(null);

export function InstanceProgressProvider({ children }: { children: ReactNode }) {
  const [state, dispatch] = useReducer(progressReducer, initialProgressState);

  useEffect(() => {
    const source = new EventSource('/api/instances/events');

    source.addEventListener('progress', (evt) => {
      try {
        const event = JSON.parse((evt as MessageEvent).data) as InstanceProgressEvent;
        dispatch({ type: 'progress', event });
      } catch {
        // Not every line has to parse; ignore what we can't.
      }
    });
    // A reconnect can miss whatever changed while disconnected; starting
    // clean is safer than showing stale progress that never gets corrected
    // because nothing changes again after reconnecting.
    source.addEventListener('error', () => dispatch({ type: 'reset' }));

    return () => source.close();
  }, []);

  return <InstanceProgressContext.Provider value={state}>{children}</InstanceProgressContext.Provider>;
}

/** `null` when nothing is actively happening to this instance (or nothing
 *  has been received about it yet — the two are indistinguishable and that
 *  is fine, since the ui treats both as "show nothing extra").
 *
 *  An update in flight is read straight from the replicated state, which is
 *  pushed the moment each step lands, rather than waiting for the progress
 *  stream to recompute every instance's summary. */
export function useInstanceProgress(instanceId: string): InstanceProgress | null {
  const ctx = useContext(InstanceProgressContext);
  if (!ctx) throw new Error('useInstanceProgress must be used inside InstanceProgressProvider');
  const update = useClusterState()?.instance_updates?.[instanceId];
  const fromUpdate = update ? updateProgress(update) : null;
  return fromUpdate ?? ctx.byId[instanceId] ?? null;
}
