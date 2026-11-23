import { fireEvent, render, screen } from '@testing-library/react';
import { MemoryRouter, Route, Routes, useLocation } from 'react-router-dom';
import { describe, expect, it, vi } from 'vitest';
import { ConfirmProvider, ToastProvider } from '../../ds';
import type { ClusterConfig } from '../../api/types';
import { SettingsNodes } from './SettingsNodes';
import type { SettingsConfig } from './SettingsConfig';

vi.mock('../../auth/AuthContext', () => ({ useAuth: () => ({ isAdmin: true }) }));
vi.mock('../../state/store', () => ({ useClusterState: () => null }));
vi.mock('../../api/client', () => ({
  api: { getNodeInterfaces: vi.fn().mockResolvedValue([]) },
  ApiError: class ApiError extends Error {},
  encodeText: (s: string) => s,
}));

const node = (ip: string) => ({
  id: 'id-' + ip,
  role: 'manager' as const,
  network: { internal: ip, wan: { ipv4: '1.2.3.4', ipv6: null, domain: null, open_ports: [{ port: 80, protocol: 'tcp' as const }, { port: 443, protocol: 'tcp' as const }] } },
  drives: { fast: '/srv/fast' },
  rpc_port: 7373,
  data_dir: null,
  docker_data_dir: null,
  generate_secret_key: null,
  last_resort: false,
});

const draft = {
  networks: { internal: { type: 'internal', subnet: '10.10.0.0/24', domain: null }, wan: { type: 'public' } },
  drives: { fast: { sync_interval: '5m', mode: 'replicated', served_by: [] } },
  nodes: { n1: node('10.10.0.1'), n2: node('10.10.0.2') },
} as unknown as ClusterConfig;

function Where() {
  return <span data-testid="where">{useLocation().pathname}</span>;
}

function renderAt(path: string, setDraft = vi.fn()) {
  const cfg = { draft, setDraft, busy: false } as unknown as SettingsConfig;
  render(
    <MemoryRouter initialEntries={[path]}>
      <ToastProvider>
        <ConfirmProvider>
          <Routes>
            <Route path="/settings/nodes/:node?" element={<><Where /><NodeParam cfg={cfg} /></>} />
          </Routes>
        </ConfirmProvider>
      </ToastProvider>
    </MemoryRouter>,
  );
  return setDraft;
}

import { useParams } from 'react-router-dom';
function NodeParam({ cfg }: { cfg: SettingsConfig }) {
  return <SettingsNodes cfg={cfg} node={useParams().node} />;
}

describe('SettingsNodes', () => {
  it('defaults to the first node', () => {
    renderAt('/settings/nodes');
    expect(screen.getByTestId('where')).toHaveTextContent('/settings/nodes/n1');
    expect(screen.getByRole('tab', { name: 'n1', selected: true })).toBeInTheDocument();
  });

  it('opens General only, with networks and drives collapsed', () => {
    renderAt('/settings/nodes/n2');
    expect(screen.getByLabelText(/cluster rpc port/i)).toHaveValue(7373);
    expect(screen.getByRole('button', { name: /^Drives/ })).toHaveAttribute('aria-expanded', 'false');
    expect(screen.queryByLabelText(/public ipv4/i)).not.toBeInTheDocument();
    expect(screen.getByText(/1\.2\.3\.4 · 80\/tcp, 443\/tcp/)).toBeInTheDocument();
  });

  it('expands a network row to edit it', () => {
    renderAt('/settings/nodes/n1');
    fireEvent.click(screen.getByRole('button', { name: /^wan/ }));
    expect(screen.getByLabelText(/public ipv4/i)).toHaveValue('1.2.3.4');
  });

  it('writes edits into the draft', () => {
    const setDraft = renderAt('/settings/nodes/n1');
    fireEvent.change(screen.getByLabelText(/cluster rpc port/i), { target: { value: '7400' } });
    expect(setDraft).toHaveBeenCalled();
    expect((setDraft.mock.calls[0][0] as ClusterConfig).nodes.n1.rpc_port).toBe(7400);
  });

  it('switches node through the sub-tabs', () => {
    renderAt('/settings/nodes/n1');
    fireEvent.click(screen.getByRole('tab', { name: 'n2' }));
    expect(screen.getByTestId('where')).toHaveTextContent('/settings/nodes/n2');
  });
});
