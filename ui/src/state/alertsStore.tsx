// The alert feed for the whole app. The rail badge and the Alerts page read
// this one poll (every 15 s, and again whenever the window regains focus), so
// they always agree and nothing polls twice.

import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import { api, ApiError } from '../api/client';
import type { AlertsResponse } from '../api/types';
import { countNew, loadReadAt, saveReadAt } from '../lib/alerts';

const POLL_MS = 15_000;

export interface AlertsState {
  /** `null` until the first answer; `undefined` on a server without alerts. */
  data: AlertsResponse | null | undefined;
  error: string | null;
  /** Unix seconds of the last "mark all as read". */
  readAt: number;
  unread: number;
  refresh: () => void;
  markAllRead: () => void;
}

const Ctx = createContext<AlertsState | null>(null);

export function AlertsProvider({ children }: { children: ReactNode }) {
  const [data, setData] = useState<AlertsResponse | null | undefined>(null);
  const [error, setError] = useState<string | null>(null);
  const [readAt, setReadAt] = useState(loadReadAt);
  const [tick, setTick] = useState(0);

  useEffect(() => {
    let off = false;
    const load = () =>
      api.alerts().then(
        (r) => {
          if (off) return;
          setData(r);
          setError(null);
        },
        (e) => {
          if (off) return;
          if (e instanceof ApiError && e.status === 404) setData(undefined);
          else setError(e instanceof ApiError ? e.message : 'Could not load alerts.');
        },
      );
    void load();
    const id = window.setInterval(load, POLL_MS);
    const onFocus = () => void load();
    window.addEventListener('focus', onFocus);
    return () => {
      off = true;
      window.clearInterval(id);
      window.removeEventListener('focus', onFocus);
    };
  }, [tick]);

  const refresh = useCallback(() => setTick((t) => t + 1), []);
  const markAllRead = useCallback(() => {
    const now = Math.floor(Date.now() / 1000);
    saveReadAt(now);
    setReadAt(now);
  }, []);

  const value = useMemo<AlertsState>(() => {
    const all = data ? [...data.active, ...data.recent] : [];
    return { data, error, readAt, unread: countNew(all, readAt), refresh, markAllRead };
  }, [data, error, readAt, refresh, markAllRead]);

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

const EMPTY: AlertsState = { data: null, error: null, readAt: 0, unread: 0, refresh: () => {}, markAllRead: () => {} };

/** Outside a provider (a unit test of one view) it is empty rather than throwing. */
export function useAlerts(): AlertsState {
  return useContext(Ctx) ?? EMPTY;
}

/** What the nav badge shows: warnings and criticals that are still happening. */
export function useAlertBadge(): { count: number; critical: boolean } {
  const { data } = useAlerts();
  return {
    count: (data?.counts.critical ?? 0) + (data?.counts.warning ?? 0),
    critical: (data?.counts.critical ?? 0) > 0,
  };
}
