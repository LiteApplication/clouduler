import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ConfirmProvider, ToastProvider } from '../../ds';
import { RollbackRelease } from './RollbackRelease';

const api = vi.hoisted(() => ({
  selfUpdateLatest: vi.fn(),
  selfUpdateReleases: vi.fn(),
  startSelfUpdate: vi.fn(),
}));
vi.mock('../../api/client', () => ({ ApiError: class extends Error {}, api }));

function renderIt() {
  return render(
    <ToastProvider>
      <ConfirmProvider>
        <RollbackRelease />
      </ConfirmProvider>
    </ToastProvider>,
  );
}

describe('RollbackRelease', () => {
  beforeEach(() => {
    Object.values(api).forEach((m) => m.mockReset());
    api.selfUpdateLatest.mockResolvedValue({ tag: 'v0.19.0', architectures: [], source: 's', running_version: '0.18.0', fetched_at: 0 });
    api.selfUpdateReleases.mockResolvedValue({ tags: ['v0.19.0', 'v0.18.0', 'v0.17.3', 'v0.17.0'] });
    api.startSelfUpdate.mockResolvedValue({});
  });

  it('is collapsed and warns that it is unsupported', async () => {
    const { container } = renderIt();
    const details = container.querySelector('details')!;
    expect(details.open).toBe(false);
    expect(screen.getByText(/unsupported/)).toBeTruthy();
    expect(screen.getByText(/most probably break your cluster/)).toBeTruthy();
  });

  it('offers only older tags and needs the tag typed before it starts with downgrade', async () => {
    renderIt();
    await screen.findByRole('option', { name: 'v0.17.3' });
    expect(screen.queryByRole('option', { name: 'v0.19.0' })).toBeNull();
    expect(screen.queryByRole('option', { name: 'v0.18.0' })).toBeNull();
    await userEvent.selectOptions(screen.getByLabelText('Older release'), 'v0.17.3');
    await userEvent.click(screen.getByRole('button', { name: 'Roll back to 0.17.3' }));
    const confirmBtn = await screen.findByRole('button', { name: 'Roll back to v0.17.3' });
    expect((confirmBtn as HTMLButtonElement).disabled).toBe(true);
    await userEvent.type(screen.getByLabelText(/Type "v0.17.3" to confirm/), 'v0.17.3');
    await userEvent.click(confirmBtn);
    expect(api.startSelfUpdate).toHaveBeenCalledWith('v0.17.3', { downgrade: true });
  });
});
