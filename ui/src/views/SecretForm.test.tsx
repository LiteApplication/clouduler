import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import type { SecretSummary } from '../api/types';
import { SecretForm } from './SecretForm';

const rotateSecret = vi.fn().mockResolvedValue({ revision: 3, restarting: [], skipped: [] });

vi.mock('../api/client', async (orig) => ({
  ...(await orig<typeof import('../api/client')>()),
  api: { rotateSecret: (...args: unknown[]) => rotateSecret(...args), putSecret: vi.fn() },
}));

const secret: SecretSummary = {
  name: 'db',
  revision: 3,
  owner: null,
  key_fingerprint: 'abc',
  stale: 1,
  used_by: [
    {
      instance: 'app.web.0',
      stack: 'app',
      service: 'web',
      node: 'n1',
      delivery: 'env',
      target: 'DB_PASSWORD',
      running: true,
      running_revision: 2,
      freshness: 'stale',
    },
  ],
};

describe('SecretForm rotate mode', () => {
  it('warns about derived data and lists the stale instance', () => {
    render(<SecretForm rotate={secret} onDone={() => undefined} />);
    expect(screen.getByText(/does not migrate data derived/i)).toBeTruthy();
    expect(screen.getByText('app.web.0')).toBeTruthy();
    expect(screen.getByText(/stale: runs r2/)).toBeTruthy();
  });

  it('stores only by default and restarts when asked', async () => {
    render(<SecretForm rotate={secret} onDone={() => undefined} />);
    fireEvent.change(screen.getByLabelText('Value'), { target: { value: 'new' } });
    fireEvent.click(screen.getByRole('button', { name: /store only/i }));
    await waitFor(() => expect(rotateSecret).toHaveBeenCalledWith('db', { value: 'new', restart: false }));

    fireEvent.click(screen.getByLabelText(/Rotate and restart now/i));
    fireEvent.click(screen.getByRole('button', { name: /rotate and restart now/i }));
    await waitFor(() => expect(rotateSecret).toHaveBeenLastCalledWith('db', { value: 'new', restart: true }));
  });
});
