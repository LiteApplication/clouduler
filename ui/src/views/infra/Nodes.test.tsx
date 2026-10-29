import { render, screen, within } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { ConfirmProvider, ToastProvider } from '../../ds';
import { describe, expect, it, vi } from 'vitest';
import { Nodes } from './Nodes';
import { api } from '../../api/client';
import type { ClusterState, Connectivity } from '../../api/types';

const cluster: ClusterState = {
  nodes: {
    n1: { name: 'n1', role: 'manager', rpc_addr: '10.10.0.1:7373', drives: {}, availability: 'active', labels: {}, egress_networks: [], networks: [], open_ports: {}, arch: 'x86_64', binary_hash: '', node_pubkey: null, memory_total_bytes: 0 },
  },
  stacks: {},
  assignments: {},
  volumes: {},
  configs: {},
  secrets: {},
  certificates: {},
  dns_records: {},
  cert_status: {},
  dns_status: {},
  unreachable: [],
  removed: [],
  wrapped_keys: {},
  cluster_config: null,
  emergency_stack: null,
  restic_destinations: {},
  restic_targets: {},
  restic_runs: {},
  restore_requests: {},
  instance_updates: {},
  overlay: null,
  version: 1,
};

let clusterOverride: ClusterState | null = null;
vi.mock('../../state/store', () => ({
  useClusterState: () => clusterOverride ?? cluster,
}));

let configNodes: Record<string, { role: string; drives?: Record<string, string>; network?: Record<string, string> }> = {};
vi.mock('../config/draft', () => ({
  useConfigDraft: () => ({ original: { nodes: configNodes }, draft: null }),
}));

let isAdmin = false;
vi.mock('../../auth/AuthContext', () => ({
  useAuth: () => ({ isAdmin, identity: { subject: 'test', username: 'test', role: isAdmin ? 'admin' : 'viewer' }, loading: false }),
}));

function renderNodes() {
  return render(
    <MemoryRouter>
      <ToastProvider>
        <ConfirmProvider>
          <Nodes />
        </ConfirmProvider>
      </ToastProvider>
    </MemoryRouter>,
  );
}

describe('Nodes view role gating', () => {
  it('hides the cluster actions for a viewer', () => {
    isAdmin = false;
    renderNodes();
    expect(screen.queryByRole('button', { name: /add node/i })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /rebalance/i })).not.toBeInTheDocument();
    expect(screen.getAllByText('n1').length).toBeGreaterThan(0);
  });

  it('shows rebalance and add node for an admin', () => {
    isAdmin = true;
    renderNodes();
    expect(screen.getByRole('button', { name: /add node/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /rebalance cluster/i })).toBeInTheDocument();
  });
});

describe('a node declared in the config but not yet in the live roster', () => {
  it('is listed as pending instead of being invisible', () => {
    isAdmin = false;
    configNodes = {
      n1: { role: 'manager' },
      n2: { role: 'worker', drives: { bulk: '/mnt/bulk' } },
    };
    renderNodes();
    expect(screen.getAllByText('n2').length).toBeGreaterThan(0);
    expect(screen.getByText('pending')).toBeInTheDocument();
    configNodes = {};
  });
});

describe('who can reach whom', () => {
  it('renders with one node, with a self diagonal and a note about more nodes', async () => {
    isAdmin = false;
    clusterOverride = null;
    vi.spyOn(api, 'nodeConnectivity').mockResolvedValue({ reports: [{ node: 'n1', reported: true, links: [] }] });
    renderNodes();
    const table = await screen.findByRole('table', { name: 'Node connectivity' });
    expect(table).toBeInTheDocument();
    expect(screen.getByText('self')).toBeInTheDocument();
    expect(screen.getByText(/links appear here once there is more than one node/i)).toBeInTheDocument();
  });

  it('shows link state as text for unreachable, api-down and not-reported cells', async () => {
    isAdmin = false;
    const extra = (name: string, addr: string) => ({ ...cluster.nodes.n1, name, rpc_addr: addr });
    clusterOverride = { ...cluster, nodes: { ...cluster.nodes, n2: extra('n2', '10.10.0.2:7373'), n3: extra('n3', '10.10.0.3:7373') } };
    const connectivity: Connectivity = {
      reports: [
        {
          node: 'n1',
          reported: true,
          links: [
            { peer: 'n2', raft: true, api: true, raft_ms: 4 },
            { peer: 'n3', raft: false, api: false, raft_ms: null },
          ],
        },
        {
          node: 'n2',
          reported: true,
          links: [
            { peer: 'n1', raft: true, api: false, raft_ms: 2 },
            { peer: 'n3', raft: true, api: true, raft_ms: 3 },
          ],
        },
        { node: 'n3', reported: false, links: [] },
      ],
    };
    vi.spyOn(api, 'nodeConnectivity').mockResolvedValue(connectivity);
    renderNodes();
    const table = await screen.findByRole('table', { name: 'Node connectivity' });
    expect(within(table).getByText('4 ms')).toBeInTheDocument();
    expect(within(table).getByText('Unreachable')).toBeInTheDocument();
    expect(within(table).getByText('API down')).toBeInTheDocument();
    // n3 did not report, so its whole row reads as not reported.
    expect(within(table).getAllByText('No report')).toHaveLength(2);
    clusterOverride = null;
  });
});
