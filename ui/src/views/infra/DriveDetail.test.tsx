import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { DriveStatus } from '../../api/types';
import { ConfirmProvider, ToastProvider } from '../../ds';
import { DriveDetail } from './DriveDetail';

let drive: DriveStatus;

vi.mock('../../state/store', () => ({
  useClusterState: () => ({ volumes: {}, restic_targets: {}, restic_destinations: {} }),
}));
vi.mock('../../auth/AuthContext', () => ({ useAuth: () => ({ isAdmin: true }) }));
vi.mock('./Storage', () => ({ useDrives: () => ({ drives: [drive], error: null }) }));

const resolveDrive = vi.fn();
const failbackDrive = vi.fn();
vi.mock('../../api/client', () => ({
  api: {
    resolveDrive: (...a: unknown[]) => resolveDrive(...a),
    failbackDrive: (...a: unknown[]) => failbackDrive(...a),
  },
  ApiError: class ApiError extends Error {},
}));

function renderPage() {
  return render(
    <MemoryRouter initialEntries={['/storage/drives/fast']}>
      <ToastProvider>
        <ConfirmProvider>
          <Routes>
            <Route path="/storage/drives/:name" element={<DriveDetail />} />
          </Routes>
        </ConfirmProvider>
      </ToastProvider>
    </MemoryRouter>,
  );
}

const base: DriveStatus = {
  name: 'fast',
  mode: 'served',
  served_by: ['n0', 'n1'],
  active_server: 'n1',
  sync_interval: '15m',
  failback: 'manual',
  served: { holder: 'n1', epoch: 2, handover_to: null, conflict: null, failback_blocked: null },
  nodes: [{ node: 'n0', reachable: true }, { node: 'n1', reachable: true }],
};

describe('DriveDetail served drive', () => {
  beforeEach(() => {
    resolveDrive.mockReset().mockResolvedValue(undefined);
    failbackDrive.mockReset().mockResolvedValue(undefined);
    drive = base;
  });

  it('shows the fail back button and posts the request', async () => {
    renderPage();
    expect(screen.getByText('Holder')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Fail back now' }));
    await waitFor(() => expect(failbackDrive).toHaveBeenCalledWith('fast'));
  });

  it('shows a conflict banner instead and resolves after confirming', async () => {
    drive = { ...base, served: { ...base.served!, conflict: { holder: 'n1', other: 'n0', reason: 'both written' } } };
    renderPage();
    expect(screen.getByText(/were both changed while they were apart/)).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Fail back now' })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Keep n0' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Keep n0', hidden: false }, { timeout: 2000 }).catch(() => screen.getAllByRole('button', { name: 'Keep n0' })[1]));
    await waitFor(() => expect(resolveDrive).toHaveBeenCalledWith('fast', 'n0'));
  });
});
