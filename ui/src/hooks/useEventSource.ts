// Generic on-demand SSE subscription for log and stats views. Unlike the
// cluster state store, these are opened only while the view showing them is
// mounted, and each one keeps its own bounded buffer of parsed items.

import { useEffect, useRef, useState } from 'react';

export interface UseEventSourceOptions<T> {
  /** Cap on buffered items, oldest dropped first. */
  limit?: number;
  enabled?: boolean;
  /**
   * Named SSE event types to subscribe to, e.g. `["stdout", "stderr"]`.
   * Defaults to the unnamed `message` event. Every server stream in this
   * api sends a named event (`state`, `stdout`/`stderr`, `stats`, ...), so
   * a caller almost always has to list the names it cares about here rather
   * than relying on the default.
   */
  events?: string[];
  /**
   * Turns one event's parsed JSON payload plus the event's name into the
   * item this hook buffers. Defaults to the payload unchanged, which is
   * enough when the payload already has the shape `T`.
   */
  mapItem?: (data: unknown, eventType: string) => T;
}

/**
 * Subscribes to an SSE endpoint and keeps a rolling buffer of parsed items
 * from the named events it listens for.
 */
export function useEventSource<T>(url: string | null, options: UseEventSourceOptions<T> = {}) {
  const { limit = 1000, enabled = true, events = ['message'], mapItem } = options;
  const [items, setItems] = useState<T[]>([]);
  const [connected, setConnected] = useState(false);
  const limitRef = useRef(limit);
  limitRef.current = limit;
  const mapRef = useRef(mapItem);
  mapRef.current = mapItem;

  // Stringified so the effect does not re-subscribe every render when the
  // caller passes a fresh array literal.
  const eventsKey = events.join(',');

  useEffect(() => {
    if (!url || !enabled) return;
    setItems([]);
    setConnected(false);
    const source = new EventSource(url);

    const onEvent = (evt: MessageEvent) => {
      try {
        const parsed = JSON.parse(evt.data);
        const item = (mapRef.current ? mapRef.current(parsed, evt.type) : parsed) as T;
        setItems((prev) => {
          const next = [...prev, item];
          return next.length > limitRef.current ? next.slice(next.length - limitRef.current) : next;
        });
      } catch {
        // Not every line the daemon emits has to be JSON; ignore what we can't parse.
      }
    };

    source.onopen = () => setConnected(true);
    for (const type of eventsKey.split(',')) {
      source.addEventListener(type, onEvent);
    }
    source.onerror = () => setConnected(false);

    return () => source.close();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [url, enabled, eventsKey]);

  return { items, connected, clear: () => setItems([]) };
}
