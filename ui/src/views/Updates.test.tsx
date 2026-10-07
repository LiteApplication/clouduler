import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { Updates } from './Updates';
import type { ClusterState, InstanceUpdate, UpdateRow } from '../api/types';

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
vi.mock('../state/store', () => ({
  useClusterState: () => ({ instance_updates: instanceUpdates }) as unknown as ClusterState,
}));

const listUpdates = vi.fn();
const applyUpdates = vi.fn();
vi.mock('../api/client', () => ({
  ApiError: class extends Error {},
  api: {
    listUpdates: () => listUpdates(),
    applyUpdates: (ids?: string[]) => applyUpdates(ids),
    listRegistries: () => Promise.resolve([]),
    listInstances: () => Promise.resolve([]),
    pinUpdates: () => Promise.resolve([]),
  },
}));

function renderPage() {
  return render(
    <MemoryRouter>
      <Updates />
    </MemoryRouter>,
  );
}

describe('Updates page', () => {
  beforeEach(() => {
    instanceUpdates = {};
    listUpdates.mockReset();
    applyUpdates.mockReset();
    applyUpdates.mockResolvedValue({ accepted: [] });
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

  it('updates exactly the selected rows', async () => {
    listUpdates.mockResolvedValue([behind('a'), behind('b'), behind('c')]);
    renderPage();
    await screen.findByLabelText('Select a');
    await userEvent.click(screen.getByLabelText('Select a'));
    await userEvent.click(screen.getByLabelText('Select c'));
    await userEvent.click(screen.getByRole('button', { name: 'Update 2' }));
    expect(applyUpdates).toHaveBeenCalledWith(['a', 'c']);
  });

  it('shows progress for an in-flight row without locking the others', async () => {
    instanceUpdates = { a: { node: 'n1', phase: 'pulling', started_at: NOW, at: NOW } };
    listUpdates.mockResolvedValue([behind('a'), behind('b')]);
    renderPage();
    const rowBar = (await screen.findAllByRole('progressbar')).find((el) => el.getAttribute('aria-valuemax') === '4');
    expect(rowBar).toHaveAttribute('aria-valuenow', '1');
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
