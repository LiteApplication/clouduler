import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ConfirmProvider, ToastProvider } from '../../ds';
import { Updates } from './Updates';
import type { ClusterState, InstanceUpdate, UpdateRow } from '../../api/types';

const NOW = Math.floor(Date.now() / 1000);

function row(id: string, over: Partial<UpdateRow> = {}): UpdateRow {
  return {
    id,
    stack: 's',
    service: id,
    node: 'n1',
    image: `img/${id}:1`,
    running_digest: 'sha256:' + 'a'.repeat(64),
    latest_digest: 'sha256:' + 'a'.repeat(64),
    pinned_digest: null,
    update_available: false,
    pin_drift: false,
    ...over,
  };
}
const behind = (id: string) => row(id, { latest_digest: 'sha256:' + 'b'.repeat(64), update_available: true });

let instanceUpdates: Record<string, InstanceUpdate> = {};
vi.mock('../../state/store', () => ({
  useClusterState: () => ({ instance_updates: instanceUpdates }) as unknown as ClusterState,
}));

const listUpdates = vi.fn();
const applyUpdates = vi.fn();
const refreshUpdates = vi.fn();
vi.mock('../../api/client', () => ({
  ApiError: class extends Error {},
  api: {
    listUpdates: () => listUpdates(),
    applyUpdates: (ids?: string[]) => applyUpdates(ids),
    refreshUpdates: () => refreshUpdates(),
    listRegistries: () => Promise.resolve([]),
    listInstances: () => Promise.resolve([]),
    pinUpdates: () => Promise.resolve([]),
  },
}));

function renderPage() {
  return render(
    <MemoryRouter>
      <ToastProvider>
        <ConfirmProvider>
          <Updates />
        </ConfirmProvider>
      </ToastProvider>
    </MemoryRouter>,
  );
}

describe('Updates page', () => {
  beforeEach(() => {
    instanceUpdates = {};
    listUpdates.mockReset();
    applyUpdates.mockReset();
    applyUpdates.mockResolvedValue({ accepted: [] });
    refreshUpdates.mockReset();
    refreshUpdates.mockResolvedValue({ started: false });
  });

  it('shows when the registries were last checked and refreshes on request', async () => {
    listUpdates.mockResolvedValue([row('a', { checked_at: NOW - 3 * 3600 })]);
    renderPage();
    await screen.findByText(/Checked 3 hours ago/);
    listUpdates.mockResolvedValue([row('a', { checked_at: NOW })]);
    await userEvent.click(screen.getByRole('button', { name: 'Refresh' }));
    expect(refreshUpdates).toHaveBeenCalledTimes(1);
    await screen.findByText(/Checked just now/);
  });

  it('does not ask the registries when the page loads', async () => {
    listUpdates.mockResolvedValue([row('a')]);
    renderPage();
    await screen.findByText('a');
    expect(refreshUpdates).not.toHaveBeenCalled();
  });

  it('says when an auto-update will be applied and what happened', async () => {
    listUpdates.mockResolvedValue([
      behind('w'),
    ].map((r) => ({
      ...r,
      auto_update: { mode: 'apply' as const, delay_secs: 86400, outcome: 'waiting' as const, apply_at: NOW + 5 * 3600 },
    })));
    const view = renderPage();
    await screen.findByText(/auto-applies in 5 hours/);
    view.unmount();
    listUpdates.mockResolvedValue([
      row('f', {
        auto_update: { mode: 'apply', delay_secs: 0, outcome: 'failed', error: 'pull failed' },
        latest_digest: 'sha256:' + 'b'.repeat(64),
        update_available: true,
      }),
    ]);
    renderPage();
    await screen.findByText(/failed, not retried for this image \(pull failed\)/);
  });

  it('does not claim the newest image while a row is unknown', async () => {
    listUpdates.mockResolvedValue([row('a'), row('b', { error: 'registry down', latest_digest: null })]);
    renderPage();
    await screen.findByText(/1 unknown/);
    expect(screen.queryByText(/newest image/i)).not.toBeInTheDocument();
  });

  it('filters by the Behind tile and toggles back', async () => {
    listUpdates.mockResolvedValue([behind('web'), row('db')]);
    renderPage();
    await screen.findByText('db');
    await userEvent.click(screen.getByRole('button', { name: /^Behind \d/ }));
    expect(screen.getByText('web')).toBeInTheDocument();
    expect(screen.queryByText('db')).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: /^Behind \d/, pressed: true }));
    expect(screen.getByText('db')).toBeInTheDocument();
  });

  it('shows progress for an in-flight row without locking the others', async () => {
    instanceUpdates = { a: { node: 'n1', phase: 'pulling', started_at: NOW, at: NOW } };
    listUpdates.mockResolvedValue([behind('a'), behind('b')]);
    renderPage();
    expect(await screen.findByText('Downloading, step 1 of 4')).toBeInTheDocument();
    const rowB = screen.getByText('b').closest('tr')!;
    expect(within(rowB).getByRole('button', { name: /Update/ })).toBeEnabled();
  });

  it('explains an auth failure and offers the registry login', async () => {
    listUpdates.mockResolvedValue([row('a', { error: 'unauthorized: bad credentials', latest_digest: null })]);
    renderPage();
    await waitFor(() => expect(screen.getByText(/unauthorized: bad credentials/)).toBeInTheDocument());
    expect(screen.getByRole('button', { name: 'Add registry login' })).toBeInTheDocument();
  });
});
