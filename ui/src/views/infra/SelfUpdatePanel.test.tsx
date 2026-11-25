import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ConfirmProvider, ToastProvider } from '../../ds';
import { SelfUpdatePanel } from './SettingsUpdate';
import type { ClusterState, SelfUpdateRunState } from '../../api/types';

vi.mock('../../auth/AuthContext', () => ({ useAuth: () => ({ isAdmin: true }) }));
vi.mock('../../state/store', () => ({
  useClusterState: () => ({ nodes: {}, configs: {} }) as unknown as ClusterState,
}));

const api = vi.hoisted(() => ({
  selfUpdateRepo: vi.fn(),
  selfUpdateLatest: vi.fn(),
  selfUpdateReleases: vi.fn(),
  startSelfUpdate: vi.fn(),
  selfUpdateRun: vi.fn(),
  cancelSelfUpdate: vi.fn(),
}));
vi.mock('../../api/client', () => ({ ApiError: class extends Error {}, api }));

function runState(tag: string, over: Partial<SelfUpdateRunState> = {}): SelfUpdateRunState {
  return {
    id: 'run-' + tag, release_tag: tag, owner: 'o', repo: 'r', assets: {}, arch_hashes: {}, per_node: {},
    started_at: 1, finished_at: null, failure_reason: null, restart_started_at: null, target_tag: tag, ...over,
  };
}

function renderPanel() {
  return render(
    <ToastProvider>
      <ConfirmProvider>
        <SelfUpdatePanel />
      </ConfirmProvider>
    </ToastProvider>,
  );
}

describe('SelfUpdatePanel step by step', () => {
  let running = '0.17.0';
  beforeEach(() => {
    running = '0.17.0';
    Object.values(api).forEach((m) => m.mockReset());
    api.selfUpdateRepo.mockResolvedValue({ release_url: 'https://github.com/o/r', check_updates: true });
    api.selfUpdateLatest.mockImplementation(() => Promise.resolve({ tag: 'v0.19.0', architectures: [], source: 's', running_version: running, fetched_at: 0 }));
    api.selfUpdateReleases.mockResolvedValue({ tags: ['v0.19.0', 'v0.18.2', 'v0.18.0', 'v0.17.3', 'v0.17.0'] });
  });
  afterEach(() => vi.useRealTimers());

  it('shows the planned path and starts the first step with its tag', async () => {
    api.startSelfUpdate.mockResolvedValue(runState('v0.17.3'));
    renderPanel();
    await screen.findByText(/v0\.17\.3 → v0\.18\.2 → v0\.19\.0/);
    await userEvent.click(screen.getByRole('button', { name: 'Start update' }));
    await userEvent.click(await screen.findByLabelText(/Update to v0\.17\.3 only/));
    await userEvent.click(await screen.findByRole('button', { name: 'Update' }));
    expect(api.startSelfUpdate).toHaveBeenCalledWith('v0.17.3');
  });

  it('drives every step in order and stops on a failure', async () => {
    api.startSelfUpdate.mockImplementation((tag: string) => Promise.resolve(runState(tag)));
    api.selfUpdateRun.mockImplementation(() => {
      const tag = api.startSelfUpdate.mock.calls.at(-1)![0] as string;
      if (tag === 'v0.18.2') return Promise.resolve(runState(tag, { finished_at: 5, failure_reason: 'node a did not come back' }));
      running = tag.slice(1);
      return Promise.resolve(runState(tag, { finished_at: 5 }));
    });
    renderPanel();
    await screen.findByText(/v0\.17\.3 → v0\.18\.2/);
    await userEvent.click(screen.getByRole('button', { name: 'Start update' }));
    await userEvent.click(await screen.findByRole('button', { name: 'Update' }));
    await screen.findByText(/Step 1 of 3: v0\.17\.3/);
    await screen.findByText(/Step 2 of 3 \(v0\.18\.2\) failed: node a did not come back/, {}, { timeout: 15000 });
    expect(api.startSelfUpdate.mock.calls.map((c) => c[0])).toEqual(['v0.17.3', 'v0.18.2']);
  }, 20000);

  it('hides the step button when one step is enough', async () => {
    running = '0.18.0';
    api.selfUpdateReleases.mockResolvedValue({ tags: ['v0.18.2', 'v0.18.0'] });
    renderPanel();
    await screen.findByRole('button', { name: /Start update to v0\.18\.2/ });
    expect(screen.getByRole('button', { name: 'Start update to v0.18.2' })).toBeInTheDocument();
  });
});
