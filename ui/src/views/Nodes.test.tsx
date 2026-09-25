import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { describe, expect, it, vi } from 'vitest';
import { Nodes } from './Nodes';
import type { ClusterState } from '../api/types';

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

vi.mock('../state/store', () => ({
  useClusterState: () => cluster,
}));

let configNodes: Record<string, { role: string; drives?: Record<string, string>; network?: Record<string, string> }> = {};
vi.mock('./config/draft', () => ({
  useConfigDraft: () => ({ original: { nodes: configNodes }, draft: null }),
}));

let isAdmin = false;
vi.mock('../auth/AuthContext', () => ({
  useAuth: () => ({ isAdmin, identity: { subject: 'test', username: 'test', role: isAdmin ? 'admin' : 'viewer' }, loading: false }),
}));

function renderNodes() {
  return render(
    <MemoryRouter>
      <Nodes />
    </MemoryRouter>,
  );
}

describe('Nodes view role gating', () => {
  it('hides node actions and the add-node button for a viewer', () => {
    isAdmin = false;
    renderNodes();
    expect(screen.queryByRole('button', { name: /add node/i })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /drain/i })).not.toBeInTheDocument();
    expect(screen.getByText('n1')).toBeInTheDocument();
  });

  it('shows node actions and the add-node button for an admin', () => {
    isAdmin = true;
    renderNodes();
    expect(screen.getByRole('button', { name: /add node/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /drain/i })).toBeInTheDocument();
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
    expect(screen.getByText('n2')).toBeInTheDocument();
    expect(screen.getByText('pending')).toBeInTheDocument();
    configNodes = {};
  });
});
