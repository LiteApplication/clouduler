import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { describe, expect, it, vi } from 'vitest';
import { Nodes } from './Nodes';
import type { ClusterState } from '../api/types';

const cluster: ClusterState = {
  nodes: {
    n1: { name: 'n1', role: 'manager', rpc_addr: '10.10.0.1:7373', drives: {}, availability: 'active' },
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

vi.mock('../state/store', () => ({
  useClusterState: () => cluster,
}));

let isAdmin = false;
vi.mock('../auth/AuthContext', () => ({
  useAuth: () => ({ isAdmin, identity: { subject: 'test', role: isAdmin ? 'admin' : 'viewer' }, loading: false }),
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
