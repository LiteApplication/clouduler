// The mock Transport: `fetch` answered from the route table, and an
// EventSource whose streams are driven by the same in-memory MockDb.

import type { EventSourceLike, Transport } from '../api/transport';
import { HttpError, type MockDb } from './db';
import { buildHttpRoutes, buildStreamRoutes } from './routes';
import { matchRoute } from './router';

const json = (status: number, body: unknown) =>
  new Response(body === undefined ? '' : JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });

class MockEventSource implements EventSourceLike {
  onopen: ((evt: Event) => void) | null = null;
  onerror: ((evt: Event) => void) | null = null;
  onmessage: ((evt: MessageEvent) => void) | null = null;
  private listeners = new Map<string, Set<(evt: MessageEvent) => void>>();
  private teardown: (() => void) | null = null;
  private closed = false;

  constructor(url: string, db: MockDb) {
    const u = new URL(url, 'http://mock.invalid');
    const matched = matchRoute(buildStreamRoutes(), u.pathname);
    // Start on the next tick so the caller can attach listeners first, as it
    // would against a real connection that takes a moment to open.
    setTimeout(() => {
      if (this.closed) return;
      if (!matched) {
        console.error(`[mock] no stream route for ${u.pathname}`);
        this.fire('error', new Event('error'));
        return;
      }
      this.fire('open', new Event('open'));
      this.teardown = matched.route.start({
        db,
        params: matched.params,
        query: u.searchParams,
        emit: (event, data) => {
          if (!this.closed) this.fire(event, new MessageEvent(event, { data: JSON.stringify(data) }));
        },
      });
      if (this.closed) this.teardown();
    }, 0);
  }

  private fire(type: string, evt: Event) {
    if (type === 'open') this.onopen?.(evt);
    if (type === 'error') this.onerror?.(evt);
    for (const l of this.listeners.get(type) ?? []) l(evt as MessageEvent);
  }

  addEventListener(type: string, listener: (evt: MessageEvent) => void) {
    if (!this.listeners.has(type)) this.listeners.set(type, new Set());
    this.listeners.get(type)!.add(listener);
  }

  removeEventListener(type: string, listener: (evt: MessageEvent) => void) {
    this.listeners.get(type)?.delete(listener);
  }

  close() {
    this.closed = true;
    this.teardown?.();
    this.teardown = null;
  }
}

export interface MockTransportOptions {
  /** Simulated round trip, so spinners are visible in the docs. */
  latencyMs?: number;
}

export function createMockTransport(db: MockDb, opts: MockTransportOptions = {}): Transport {
  const routes = buildHttpRoutes();
  return {
    async fetch(input, init) {
      const method = (init?.method ?? 'GET').toUpperCase();
      const u = new URL(input, 'http://mock.invalid');
      if (opts.latencyMs) await new Promise((r) => setTimeout(r, opts.latencyMs));
      const matched = matchRoute(
        routes.filter((r) => r.method === method),
        u.pathname,
      );
      if (!matched) {
        const msg = `mock backend has no route for ${method} ${u.pathname}`;
        console.error(`[mock] ${msg}`);
        return json(501, { error: msg });
      }
      let body: unknown;
      if (typeof init?.body === 'string' && init.body) {
        try {
          body = JSON.parse(init.body);
        } catch {
          return json(400, { error: 'request body is not json' });
        }
      }
      try {
        const out = matched.route.handler({ db, params: matched.params, query: u.searchParams, body });
        return json(200, await out);
      } catch (e) {
        if (e instanceof HttpError) return json(e.status, { error: e.message });
        console.error(`[mock] ${method} ${u.pathname} handler crashed`, e);
        return json(500, { error: `mock handler crashed: ${e instanceof Error ? e.message : String(e)}` });
      }
    },
    eventSource: (url) => new MockEventSource(url, db),
  };
}
