// Generic on-demand SSE subscription for log and stats views. Unlike the
// cluster state store, these are opened only while the view showing them is
// mounted, and each one keeps its own bounded buffer of parsed items.

import { useEffect, useRef, useState } from 'react';

export interface UseEventSourceOptions {
  /** Cap on buffered items, oldest dropped first. */
  limit?: number;
  enabled?: boolean;
}

/**
 * Subscribes to an SSE endpoint that emits one JSON payload per unnamed
 * `message` event (the shape logs and stats streams use), and keeps a
 * rolling buffer of the parsed items.
 */
export function useEventSource<T>(url: string | null, options: UseEventSourceOptions = {}) {
  const { limit = 1000, enabled = true } = options;
  const [items, setItems] = useState<T[]>([]);
  const [connected, setConnected] = useState(false);
  const limitRef = useRef(limit);
  limitRef.current = limit;

  useEffect(() => {
    if (!url || !enabled) return;
    setItems([]);
    setConnected(false);
    const source = new EventSource(url);

    source.onopen = () => setConnected(true);
    source.onmessage = (evt) => {
      try {
        const parsed = JSON.parse(evt.data) as T;
        setItems((prev) => {
          const next = [...prev, parsed];
          return next.length > limitRef.current ? next.slice(next.length - limitRef.current) : next;
        });
      } catch {
        // Not every line the daemon emits has to be JSON; ignore what we can't parse.
      }
    };
    source.onerror = () => setConnected(false);

    return () => source.close();
  }, [url, enabled]);

  return { items, connected, clear: () => setItems([]) };
}
