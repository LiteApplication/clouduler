// A container's live stats as a rolling client-side history for sparklines.
// The server only streams the latest sample (`/api/instances/:id/stats`, event
// `stats`), so history starts when the page opens.

import { useEventSource } from './useEventSource';
import type { InstanceStatsSample } from '../api/types';

export const STATS_RING = 120;

export function useStatsHistory(instanceId: string | null, enabled = true) {
  const { items, connected } = useEventSource<InstanceStatsSample>(
    instanceId ? `/api/instances/${encodeURIComponent(instanceId)}/stats` : null,
    { limit: STATS_RING, enabled, events: ['stats'] },
  );
  const latest = items.length > 0 ? items[items.length - 1] : null;
  return {
    samples: items,
    latest,
    connected,
    cpu: items.map((s) => s.cpu_percent),
    memory: items.map((s) => s.memory_used_bytes),
    /** Bytes per sample interval, derived from the cumulative counters. */
    netRx: deltas(items.map((s) => s.network_rx_bytes)),
    netTx: deltas(items.map((s) => s.network_tx_bytes)),
    ioRead: deltas(items.map((s) => s.io_read_bytes)),
    ioWrite: deltas(items.map((s) => s.io_write_bytes)),
  };
}

function deltas(cumulative: number[]): number[] {
  return cumulative.slice(1).map((v, i) => Math.max(0, v - cumulative[i]));
}
