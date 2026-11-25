import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ConfirmProvider, ToastProvider } from '../../ds';
import { GitBackupRollback } from './sections/GitBackupRollback';

const auth = vi.hoisted(() => ({ isAdmin: true }));
vi.mock('../../auth/AuthContext', () => ({ useAuth: () => auth }));

const api = vi.hoisted(() => ({ gitBackupCommits: vi.fn(), gitBackupRollback: vi.fn() }));
vi.mock('../../api/client', () => ({
  ApiError: class extends Error {
    status = 409;
  },
  api,
}));

const commits = [
  { sha: 'a'.repeat(40), short: 'a'.repeat(12), message: 'update stack web', author: 'clouduler', time: 1 },
  { sha: 'b'.repeat(40), short: 'b'.repeat(12), message: 'update cluster.yaml', author: 'clouduler', time: 2 },
];

function renderIt(configured = true) {
  return render(
    <ToastProvider>
      <ConfirmProvider>
        <GitBackupRollback configured={configured} />
      </ConfirmProvider>
    </ToastProvider>,
  );
}

describe('GitBackupRollback', () => {
  beforeEach(() => {
    auth.isAdmin = true;
    Object.values(api).forEach((m) => m.mockReset());
    api.gitBackupCommits.mockResolvedValue(commits);
  });

  it('renders nothing when git backup is off or the user is not admin', () => {
    const a = renderIt(false);
    expect(screen.queryByText(/Advanced: roll back/)).toBeNull();
    a.unmount();
    auth.isAdmin = false;
    const b = renderIt(true);
    expect(screen.queryByText(/Advanced: roll back/)).toBeNull();
    b.unmount();
  });

  it('only loads commits once opened', async () => {
    renderIt();
    expect(api.gitBackupCommits).not.toHaveBeenCalled();
    await userEvent.click(screen.getByText(/Advanced: roll back/));
    expect(await screen.findByText('update stack web')).toBeInTheDocument();
    expect(api.gitBackupCommits).toHaveBeenCalledTimes(1);
  });

  it('requires typing the short sha, then rolls back', async () => {
    api.gitBackupRollback.mockResolvedValue({ commit: 'c'.repeat(40) });
    renderIt();
    await userEvent.click(screen.getByText(/Advanced: roll back/));
    await userEvent.click((await screen.findAllByRole('button', { name: /Roll back to/ }))[0]);
    await userEvent.type(await screen.findByRole('textbox'), 'a'.repeat(12));
    await userEvent.click(await screen.findByRole('button', { name: 'Roll back' }));
    expect(api.gitBackupRollback).toHaveBeenCalledWith('a'.repeat(40));
    expect(await screen.findByText(/new commit cccccccccccc/)).toBeInTheDocument();
  });

  it('shows the server message when the rollback is rejected', async () => {
    const { ApiError } = await import('../../api/client');
    api.gitBackupRollback.mockRejectedValue(new (ApiError as unknown as new (m: string) => Error)('pull rejected: bad yaml'));
    renderIt();
    await userEvent.click(screen.getByText(/Advanced: roll back/));
    await userEvent.click((await screen.findAllByRole('button', { name: /Roll back to/ }))[1]);
    await userEvent.type(await screen.findByRole('textbox'), 'b'.repeat(12));
    await userEvent.click(await screen.findByRole('button', { name: 'Roll back' }));
    expect(await screen.findByText(/pull rejected: bad yaml/)).toBeInTheDocument();
  });
});
