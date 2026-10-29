// Polls per-node CPU/memory/disk every 5 s for the node cards, tables and
// sparklines. `GET /api/nodes/metrics` is one call for the whole cluster; on an
// older daemon (404) it falls back to `GET /api/nodes/{name}` per known node,
// which only has real numbers for the answering node. Samples are also kept in
// a client-side ring, so a sparkline fills in while the page is open.

import { useEffect, useRef, useState } from 'react';
import { api, ApiError } from '../api/client';
import type { NodeMetrics, ResourceUsage } from '../api/types';

export const NODE_METRICS_POLL_MS = 5000;
export const NODE_METRICS_RING = 120;

export interface NodeSample {
  cpu: number;
  mem: number;
}

export interface UseNodeMetrics {
  /** Latest usage per node; a node missing here is unreachable or not yet answered. */
  metrics: NodeMetrics;
  /** Per node, newest last, capped at 120 samples. */
  history: Record<string, NodeSample[]>;
  loaded: boolean;
}

export function memPercent(u: ResourceUsage): number | null {
  return u.memory_total_bytes > 0 ? (u.memory_used_bytes / u.memory_total_bytes) * 100 : null;
}

/** CPU percent when the daemon reports it, else load/cores as a rough stand-in. */
export function cpuPercent(u: ResourceUsage): number | null {
  if (u.cpu_percent != null) return u.cpu_percent;
  return u.cpu_count > 0 && u.local ? Math.min(100, (u.load_1m / u.cpu_count) * 100) : null;
}

export function useNodeMetrics(nodeNames: string[], enabled = true): UseNodeMetrics {
  const [state, setState] = useState<UseNodeMetrics>({ metrics: {}, history: {}, loaded: false });
  const names = useRef(nodeNames);
  names.current = nodeNames;
  const perNodeFallback = useRef(false);

  useEffect(() => {
    if (!enabled) return;
    let stopped = false;

    const fetchAll = async (): Promise<NodeMetrics> => {
      if (!perNodeFallback.current) {
        try {
          return await api.nodeMetrics();
        } catch (e) {
          if (!(e instanceof ApiError && e.status === 404)) throw e;
          perNodeFallback.current = true;
        }
      }
      const out: NodeMetrics = {};
      await Promise.all(
        names.current.map(async (n) => {
          try {
            out[n] = (await api.getNode(n)).usage;
          } catch {
            // An unreachable node simply has no sample this round.
          }
        }),
      );
      return out;
    };

    const tick = async () => {
      let metrics: NodeMetrics;
      try {
        metrics = await fetchAll();
      } catch {
        return;
      }
      if (stopped) return;
      setState((prev) => {
        const history = { ...prev.history };
        for (const [name, u] of Object.entries(metrics)) {
          const cpu = cpuPercent(u);
          const mem = memPercent(u);
          if (cpu == null && mem == null) continue;
          const ring = [...(history[name] ?? []), { cpu: cpu ?? 0, mem: mem ?? 0 }];
          history[name] = ring.length > NODE_METRICS_RING ? ring.slice(ring.length - NODE_METRICS_RING) : ring;
        }
        return { metrics, history, loaded: true };
      });
    };

    void tick();
    const id = window.setInterval(() => void tick(), NODE_METRICS_POLL_MS);
    return () => {
      stopped = true;
      window.clearInterval(id);
    };
  }, [enabled]);

  return state;
}
