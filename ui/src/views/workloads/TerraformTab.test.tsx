import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ConfirmProvider, ToastProvider } from '../../ds';
import { TerraformTab } from './TerraformTab';
import type { TerraformStatus } from '../../api/types';

const getStackTerraform = vi.fn();
const putTerraformModule = vi.fn();
const runTerraform = vi.fn();
const deleteTerraformModule = vi.fn();
vi.mock('../../api/client', () => ({
  ApiError: class extends Error {},
  api: {
    getStackTerraform: (n: string) => getStackTerraform(n),
    putTerraformModule: (...a: unknown[]) => putTerraformModule(...a),
    runTerraform: (...a: unknown[]) => runTerraform(...a),
    deleteTerraformModule: (...a: unknown[]) => deleteTerraformModule(...a),
  },
}));

const SPEC = 'services:\n  web:\n    image: nginx\n  db:\n    image: postgres\n';

function status(over: Partial<TerraformStatus> = {}): TerraformStatus {
  return {
    enabled_cluster: true,
    services: [
      {
        service: 'web',
        source: 'resource "null_resource" "a" {}\n',
        revision: 2,
        enabled: true,
        last_run: {
          id: 'r1',
          mode: 'apply',
          trigger: 'manual',
          node: 'n1',
          instance: 'app.web.0',
          fingerprint: 'f',
          started_at: 1700000000,
          finished_at: 1700000010,
          outcome: 'failed',
          message: 'boom',
          log_tail: 'Error: something broke',
        },
        pending_request: null,
        outputs: [],
      },
    ],
    ...over,
  };
}

function renderTab() {
  return render(
    <MemoryRouter>
      <ToastProvider>
        <ConfirmProvider>
          <TerraformTab name="app" spec={SPEC} isAdmin />
        </ConfirmProvider>
      </ToastProvider>
    </MemoryRouter>,
  );
}

describe('TerraformTab', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    getStackTerraform.mockResolvedValue(status());
    putTerraformModule.mockResolvedValue(undefined);
    runTerraform.mockResolvedValue(undefined);
  });

  it('shows a card per spec service with the last run', async () => {
    renderTab();
    expect(await screen.findByText('Failed')).toBeInTheDocument();
    expect(screen.getByText('web')).toBeInTheDocument();
    expect(screen.getByText('db')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Add module' })).toBeInTheDocument();
    expect(screen.getByText('Error: something broke')).toBeInTheDocument();
  });

  it('lists the objects a module wrote, with revisions', async () => {
    const base = status();
    getStackTerraform.mockResolvedValue({
      ...base,
      services: [
        {
          ...base.services[0],
          outputs: [
            { kind: 'secret', name: 'grafana-oidc-secret', revision: 3 },
            { kind: 'config', name: 'grafana-oidc-id', revision: 1 },
          ],
        },
      ],
    });
    renderTab();
    expect(await screen.findByText('Writes')).toBeInTheDocument();
    expect(screen.getByText('grafana-oidc-secret')).toBeInTheDocument();
    expect(screen.getByText('(rev 3)')).toBeInTheDocument();
    expect(screen.getByText('grafana-oidc-id')).toBeInTheDocument();
  });

  it('requests a plan', async () => {
    renderTab();
    await userEvent.click(await screen.findByRole('button', { name: 'Plan' }));
    expect(runTerraform).toHaveBeenCalledWith('app', 'web', 'plan');
  });

  it('saves an edited module', async () => {
    renderTab();
    const box = await screen.findByLabelText(/Module for web/);
    await userEvent.type(box, '# hi');
    await userEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(putTerraformModule).toHaveBeenCalledWith('app', 'web', expect.stringContaining('# hi')));
  });

  it('disables plan and apply and links to settings when the cluster has terraform off', async () => {
    getStackTerraform.mockResolvedValue(status({ enabled_cluster: false }));
    renderTab();
    expect(await screen.findByText(/disabled in the cluster settings/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Plan' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Apply' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Open settings' })).toBeInTheDocument();
  });
});
