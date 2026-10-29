// Live instance states (running, exited, unknown...) for the whole app. The
// cluster store only has assignments, not what each container is doing, so
// anything that needs "which stacks are failing" - the rail badge, the
// overview - reads this one poll instead of every view polling on its own.

import { createContext, useContext, useEffect, useState, type ReactNode } from 'react';
import { api } from '../api/client';
import type { InstanceState } from '../api/types';

const POLL_MS = 5000;

const InstancesContext = createContext<InstanceState[] | null | undefined>(undefined);

export function InstancesProvider({ children }: { children: ReactNode }) {
  const [instances, setInstances] = useState<InstanceState[] | null>(null);

  useEffect(() => {
    let cancelled = false;
    const load = () =>
      api
        .listInstances()
        .then((list) => !cancelled && setInstances(list))
        .catch(() => {});
    void load();
    const id = window.setInterval(load, POLL_MS);
    return () => {
      cancelled = true;
      window.clearInterval(id);
    };
  }, []);

  return <InstancesContext.Provider value={instances}>{children}</InstancesContext.Provider>;
}

/** `null` until the first answer. Outside a provider (a unit test of one
 *  view) it is `null` too rather than throwing. */
export function useInstances(): InstanceState[] | null {
  return useContext(InstancesContext) ?? null;
}

/** An instance id is "stack.service.index". */
export function stackOf(id: string): string {
  return id.split('.')[0];
}

/** Stacks with at least one instance that crashed or could not be placed. */
export function failedStacks(instances: InstanceState[] | null): Set<string> {
  const out = new Set<string>();
  for (const i of instances ?? []) {
    if (i.state === 'exited' || i.state === 'pending_placement') out.add(stackOf(i.id));
  }
  return out;
}
