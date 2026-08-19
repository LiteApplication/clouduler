import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { NodeDetail } from './NodeDetail';
import { ConfigDraftProvider } from './config/draft';
import type { ClusterConfig, ClusterState } from '../api/types';

const cluster: ClusterState = {
  nodes: {
    n1: { name: 'n1', role: 'manager', rpc_addr: '10.10.0.1:7373', drives: { fast: '/srv/fast' }, availability: 'active', egress_networks: [], networks: ['internal'], open_ports: {}, arch: 'x86_64', binary_hash: '' },
  },
  stacks: {},
  assignments: {},
  volumes: {},
  configs: {},
  secrets: {},
  certificates: {},
  dns_records: {},
  unreachable: [],
  overlay: null,
  version: 1,
};

const clusterConfig: ClusterConfig = {
  auth: { api_keys: true, session_ttl: '12h', oidc: null },
  cluster: { name: 'test' },
  data_dir: '/var/lib/clouduler',
  drives: { fast: { sync_interval: '5m', mode: 'replicated', served_by: null } },
  failover: { fence_timeout: '15s', node_timeout: '30s', probe_interval: '5s' },
  networks: { internal: { type: 'internal', subnet: '10.10.0.0/24', domain: null } },
  nodes: {
    n1: {
      role: 'manager',
      network: { internal: '10.10.0.1' },
      drives: { fast: '/srv/fast' },
      rpc_port: 7373,
      data_dir: null,
      generate_secret_key: null,
    },
  },
  proxy: { acme: { enabled: false }, enabled: true, http_port: 80, https_port: 443 },
  secrets: { generate_if_missing: true, key_file: '/etc/clouduler/secrets.key' },
  web: { address: 'cloud', backend_port: 8444, network: 'internal', port: 8443, through_proxy: true },
  cloudflare: null,
  self_update: null,
};

vi.mock('../state/store', () => ({
  useClusterState: () => cluster,
}));

let isAdmin = true;
vi.mock('../auth/AuthContext', () => ({
  useAuth: () => ({ isAdmin, identity: { subject: 'test', role: isAdmin ? 'admin' : 'viewer' }, loading: false }),
}));

const getNode = vi.fn();
const getConfiguration = vi.fn();
const pendingConfiguration = vi.fn();
const validateConfiguration = vi.fn();
const applyConfiguration = vi.fn();

vi.mock('../api/client', () => ({
  api: {
    getNode: (...args: unknown[]) => getNode(...args),
    getConfiguration: (...args: unknown[]) => getConfiguration(...args),
    pendingConfiguration: (...args: unknown[]) => pendingConfiguration(...args),
    validateConfiguration: (...args: unknown[]) => validateConfiguration(...args),
    applyConfiguration: (...args: unknown[]) => applyConfiguration(...args),
    setAvailability: vi.fn(),
    removeNode: vi.fn(),
  },
  ApiError: class ApiError extends Error {},
}));

function renderNodeDetail() {
  return render(
    <MemoryRouter initialEntries={['/nodes/n1']}>
      <ConfigDraftProvider>
        <Routes>
          <Route path="/nodes/:name" element={<NodeDetail />} />
        </Routes>
      </ConfigDraftProvider>
    </MemoryRouter>,
  );
}

describe('NodeDetail configuration section', () => {
  beforeEach(() => {
    isAdmin = true;
    getNode.mockResolvedValue({
      name: 'n1',
      role: 'manager',
      availability: 'active',
      reachable: true,
      drives: ['fast'],
      container_count: 0,
      usage: { local: false, cpu_count: 0, load_1m: 0, memory_total_bytes: 0, memory_used_bytes: 0, uptime_seconds: 0 },
    });
    getConfiguration.mockResolvedValue(clusterConfig);
    pendingConfiguration.mockResolvedValue({ armed: false, seconds_left: null });
  });

  it('shows the node config read-only with an Edit button, admin only', async () => {
    renderNodeDetail();
    await waitFor(() => expect(screen.getByText('manager')).toBeInTheDocument());
    expect(screen.getByText('10.10.0.1')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /edit/i })).toBeInTheDocument();
    // The editing form is not mounted until Edit is clicked.
    expect(screen.queryByRole('button', { name: /save/i })).not.toBeInTheDocument();
  });

  it('hides the Edit button for a viewer', async () => {
    isAdmin = false;
    renderNodeDetail();
    await waitFor(() => expect(screen.getByText('manager')).toBeInTheDocument());
    expect(screen.queryByRole('button', { name: /edit/i })).not.toBeInTheDocument();
  });

  it('entering edit mode shows the node card form with Save and Cancel', async () => {
    renderNodeDetail();
    await waitFor(() => expect(screen.getByRole('button', { name: /edit/i })).toBeInTheDocument());
    fireEvent.click(screen.getByRole('button', { name: /edit/i }));

    expect(screen.getByRole('button', { name: /save/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /cancel/i })).toBeInTheDocument();
    // The NodeCard editing form exposes the RPC port as a number field.
    expect(screen.getByLabelText(/rpc port/i)).toHaveValue(7373);
  });

  it('cancelling edit mode returns to the read-only view without saving', async () => {
    renderNodeDetail();
    await waitFor(() => expect(screen.getByRole('button', { name: /edit/i })).toBeInTheDocument());
    fireEvent.click(screen.getByRole('button', { name: /edit/i }));
    expect(screen.getByRole('button', { name: /save/i })).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /cancel/i }));
    expect(screen.queryByRole('button', { name: /save/i })).not.toBeInTheDocument();
    expect(applyConfiguration).not.toHaveBeenCalled();
  });
});
