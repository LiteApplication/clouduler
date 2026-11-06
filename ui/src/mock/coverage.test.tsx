// Every method of the ui's `api` object must have a mock route, and every
// scenario must render the real app. Together with contract.test.ts these are
// what keep the docs-site mock from silently rotting.

import { render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import { App } from '../App';
import { api, ApiError } from '../api/client';
import { setTransport } from '../api/transport';
import { AuthProvider } from '../auth/AuthContext';
import { installMock } from './install';
import { buildScenario, scenarios } from './scenarios';
import { createMockTransport } from './transport';

beforeAll(() => {
  // jsdom has no matchMedia; the theme hook asks for it.
  if (!window.matchMedia) {
    window.matchMedia = ((query: string) => ({
      matches: false,
      media: query,
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
      addListener: () => undefined,
      removeListener: () => undefined,
      onchange: null,
      dispatchEvent: () => false,
    })) as unknown as typeof window.matchMedia;
  }
  Element.prototype.scrollIntoView ??= () => undefined;
});

afterEach(() => {
  setTransport();
  vi.restoreAllMocks();
});

describe('mock covers the whole api client', () => {
  it('answers every method of `api` with something other than 501', async () => {
    setTransport(createMockTransport(buildScenario('lan-3-nodes')));
    const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const unrouted: string[] = [];
    const entries = Object.entries(api) as [string, (...args: unknown[]) => Promise<unknown>][];
    expect(entries.length).toBeGreaterThan(100);
    for (const [name, fn] of entries) {
      try {
        await fn(...Array.from({ length: fn.length }, () => 'dummy'));
      } catch (e) {
        // A 4xx for nonsense arguments is fine; 501 means no route exists.
        if (e instanceof ApiError && e.status === 501) unrouted.push(`${name}: ${e.message}`);
        else if (!(e instanceof ApiError)) throw e;
      }
    }
    expect(unrouted).toEqual([]);
    const mockErrors = errors.mock.calls.filter((c) => String(c[0]).startsWith('[mock]'));
    expect(mockErrors).toEqual([]);
  });

  it('answers an unknown route loudly, with a 501 naming the path', async () => {
    setTransport(createMockTransport(buildScenario('lan-3-nodes')));
    const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const res = await fetch_('/api/does-not-exist');
    expect(res.status).toBe(501);
    expect(await res.text()).toContain('/api/does-not-exist');
    expect(errors).toHaveBeenCalled();
  });

  it('mutations are visible to later reads', async () => {
    setTransport(createMockTransport(buildScenario('fresh-single-node')));
    await api.deployStack('demo', { spec: 'services:\n  web:\n    image: nginx:1.27\n' });
    expect((await api.state()).stacks.demo.revision).toBe(1);
    expect((await api.listInstances()).map((i) => i.id)).toEqual(['demo.web.0']);
    await api.removeStack('demo');
    expect(Object.keys((await api.state()).stacks)).toEqual([]);
  });
});

async function fetch_(path: string) {
  const { getTransport } = await import('../api/transport');
  return getTransport().fetch(path, { method: 'GET' });
}

const ROUTES = [
  '/',
  '/stacks',
  '/stacks/new',
  '/instances',
  '/logs',
  '/alerts',
  '/updates',
  '/nodes',
  '/proxy',
  '/dns',
  '/storage',
  '/settings',
];

for (const scenario of Object.keys(scenarios)) {
  describe(`scenario ${scenario} renders the app`, () => {
    const probe = buildScenario(scenario);
    const dynamic = [
      ...Object.keys(probe.state.stacks).slice(0, 1).map((s) => `/stacks/${s}`),
      ...Object.keys(probe.state.nodes).slice(0, 1).map((n) => `/nodes/${n}`),
      ...Object.keys(probe.state.assignments).slice(0, 1).map((i) => `/instances/${i}`),
    ];
    for (const route of [...ROUTES, ...dynamic]) {
      it(`at ${route}`, async () => {
        installMock(scenario);
        const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined);
        const { container, unmount } = render(
          <MemoryRouter initialEntries={[route]}>
            <AuthProvider>
              <App />
            </AuthProvider>
          </MemoryRouter>,
        );
        // Past the auth gate and the first state event: the nav shows the user.
        await waitFor(() => expect(screen.getAllByText('demo').length).toBeGreaterThan(0), { timeout: 3000 });
        // Some routes redirect and load data first, so wait for content rather
        // than a fixed delay.
        await waitFor(
          () => expect((container.querySelector('main')?.textContent ?? '').length).toBeGreaterThan(10),
          { timeout: 3000 },
        );
        const mockErrors = errors.mock.calls.filter((c) => String(c[0]).startsWith('[mock]'));
        expect(mockErrors).toEqual([]);
        unmount();
      });
    }
  });
}
