import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ConfirmProvider, ToastProvider } from '../../ds';
import { MigrationProblem } from './MigrationProblem';
import type { MigrationProblem as MigrationProblemType } from '../../api/types';

const retryMigration = vi.fn();
const dismissMigration = vi.fn();
const restartInstance = vi.fn();
vi.mock('../../api/client', () => ({
  ApiError: class extends Error {},
  api: {
    retryMigration: (id: string) => retryMigration(id),
    dismissMigration: (id: string) => dismissMigration(id),
    restartInstance: (id: string) => restartInstance(id),
  },
}));

function problem(over: Partial<MigrationProblemType> = {}): MigrationProblemType {
  return {
    kind: 'verify_mismatch',
    title: "The copy on n1 didn't match after the move",
    detail:
      'Files in fast/v0 changed while they were being copied. app.0 also uses this volume and kept running, so it most likely wrote to it. Nothing was switched. job.0 is still running on n0.',
    volume: 'fast/v0',
    added: [],
    changed: ['apps/activity/l10n/fr.json', 'custom_apps/calendar/js/calendar-main.js'],
    removed: [],
    files_total: 2,
    co_writers: [{ instance: 'web.app.0', node: 'n0', running: true }],
    from: 'n0',
    to: 'n1',
    raw: 'verification failed: fast/v0: target differs after sync (added=[] changed=["apps/activity/l10n/fr.json", "custom_apps/calendar/js/calendar-main.js"] removed=[])',
    actions: [
      {
        id: 'retry',
        label: 'Try moving to n1 again',
        description: 'Copies the volume again and checks it.',
        primary: true,
        danger: false,
      },
      {
        id: 'move_elsewhere',
        label: 'Move to another node',
        description: 'Pick a different node and start a new move.',
        primary: false,
        danger: false,
      },
      {
        id: 'dismiss',
        label: 'Keep it on n0',
        description: 'Clears this message and leaves the container where it is.',
        primary: false,
        danger: false,
      },
    ],
    ...over,
  };
}

function renderProblem(props: Partial<Parameters<typeof MigrationProblem>[0]> = {}) {
  const onOpenMove = props.onOpenMove ?? vi.fn();
  const onActionComplete = props.onActionComplete ?? vi.fn();
  render(
    <MemoryRouter>
      <ToastProvider>
        <ConfirmProvider>
          <MigrationProblem
            instanceId="web.job.0"
            problem={problem()}
            isAdmin
            onOpenMove={onOpenMove}
            onActionComplete={onActionComplete}
            {...props}
          />
        </ConfirmProvider>
      </ToastProvider>
    </MemoryRouter>,
  );
  return { onOpenMove, onActionComplete };
}

describe('MigrationProblem', () => {
  beforeEach(() => {
    retryMigration.mockReset().mockResolvedValue(undefined);
    dismissMigration.mockReset().mockResolvedValue(undefined);
    restartInstance.mockReset().mockResolvedValue(undefined);
  });

  it('renders the title and detail', () => {
    renderProblem();
    expect(screen.getByText("The copy on n1 didn't match after the move")).toBeInTheDocument();
    expect(screen.getByText(/Files in fast\/v0 changed while they were being copied/)).toBeInTheDocument();
  });

  it('links to a running co-writer by instance id', () => {
    renderProblem();
    const link = screen.getByRole('link', { name: /web\.app\.0/ });
    expect(link).toHaveAttribute('href', '/instances/web.app.0');
    expect(screen.getByText('running')).toBeInTheDocument();
  });

  it('keeps the changed-file list collapsed until toggled', async () => {
    renderProblem();
    expect(screen.queryByText('apps/activity/l10n/fr.json')).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: /Show differing files/ }));
    expect(screen.getByText('apps/activity/l10n/fr.json')).toBeInTheDocument();
    expect(screen.getByText('custom_apps/calendar/js/calendar-main.js')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: /Hide differing files/ }));
    expect(screen.queryByText('apps/activity/l10n/fr.json')).not.toBeInTheDocument();
  });

  it('keeps the raw technical details collapsed until toggled', async () => {
    renderProblem();
    expect(screen.queryByText(/verification failed: fast\/v0/)).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Technical details' }));
    expect(screen.getByText(/verification failed: fast\/v0/)).toBeInTheDocument();
  });

  it('hides the action buttons for non-admin viewers', () => {
    renderProblem({ isAdmin: false });
    expect(screen.queryByRole('button', { name: 'Try moving to n1 again' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Keep it on n0' })).not.toBeInTheDocument();
  });

  it('retries the migration directly, without a confirm step', async () => {
    renderProblem();
    await userEvent.click(screen.getByRole('button', { name: 'Try moving to n1 again' }));
    await waitFor(() => expect(retryMigration).toHaveBeenCalledWith('web.job.0'));
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
  });

  it('calls onActionComplete after a successful retry', async () => {
    const { onActionComplete } = renderProblem();
    await userEvent.click(screen.getByRole('button', { name: 'Try moving to n1 again' }));
    await waitFor(() => expect(onActionComplete).toHaveBeenCalled());
  });

  it('opens the move sheet via onOpenMove instead of calling the api', async () => {
    const { onOpenMove } = renderProblem();
    await userEvent.click(screen.getByRole('button', { name: 'Move to another node' }));
    expect(onOpenMove).toHaveBeenCalled();
    expect(retryMigration).not.toHaveBeenCalled();
    expect(dismissMigration).not.toHaveBeenCalled();
  });

  it('dismisses the problem', async () => {
    renderProblem();
    await userEvent.click(screen.getByRole('button', { name: 'Keep it on n0' }));
    await waitFor(() => expect(dismissMigration).toHaveBeenCalledWith('web.job.0'));
  });

  it('restarts the instance for a start-failure action', async () => {
    renderProblem({
      problem: problem({
        kind: 'start_failed',
        title: "job.0 didn't start on n1",
        actions: [
          {
            id: 'restart',
            label: 'Restart on n1',
            description: 'Tries to start the container on its new node again.',
            primary: true,
            danger: false,
          },
        ],
      }),
    });
    await userEvent.click(screen.getByRole('button', { name: 'Restart on n1' }));
    await waitFor(() => expect(restartInstance).toHaveBeenCalledWith('web.job.0'));
  });

  it('asks for confirmation before firing a danger action, and does nothing on cancel', async () => {
    renderProblem({
      problem: problem({
        actions: [
          {
            id: 'dismiss',
            label: 'Keep it on n0',
            description: 'Clears this message and leaves the container where it is.',
            primary: false,
            danger: true,
          },
        ],
      }),
    });
    await userEvent.click(screen.getByRole('button', { name: 'Keep it on n0' }));
    const dialog = await screen.findByRole('alertdialog');
    expect(dismissMigration).not.toHaveBeenCalled();
    await userEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument());
    expect(dismissMigration).not.toHaveBeenCalled();
  });

  it('fires the action once the confirmation dialog is accepted', async () => {
    renderProblem({
      problem: problem({
        actions: [
          {
            id: 'dismiss',
            label: 'Keep it on n0',
            description: 'Clears this message and leaves the container where it is.',
            primary: false,
            danger: true,
          },
        ],
      }),
    });
    await userEvent.click(screen.getByRole('button', { name: 'Keep it on n0' }));
    const dialog = await screen.findByRole('alertdialog');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Keep it on n0' }));
    await waitFor(() => expect(dismissMigration).toHaveBeenCalledWith('web.job.0'));
  });

  it('shows a danger toast when the action fails', async () => {
    retryMigration.mockRejectedValue(new Error('node unreachable'));
    renderProblem();
    await userEvent.click(screen.getByRole('button', { name: 'Try moving to n1 again' }));
    await screen.findByText(/node unreachable/);
  });
});
