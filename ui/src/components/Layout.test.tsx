import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { describe, expect, it, vi } from 'vitest';
import { Layout } from './Layout';
import { ConfigDraftProvider } from '../views/config/draft';
import type { ClusterState } from '../api/types';

const cluster: ClusterState = {
  nodes: {
    n1: { name: 'n1', role: 'manager', rpc_addr: '10.10.0.1:7373', drives: {}, availability: 'active', egress_networks: [], networks: [], open_ports: {}, arch: 'x86_64', binary_hash: '', node_pubkey: null },
    n2: { name: 'n2', role: 'worker', rpc_addr: '10.10.0.2:7373', drives: {}, availability: 'active', egress_networks: [], networks: [], open_ports: {}, arch: 'x86_64', binary_hash: '', node_pubkey: null },
  },
  stacks: {},
  assignments: {},
  volumes: {},
  configs: {},
  secrets: {},
  certificates: {},
  dns_records: {},
  unreachable: [],
  wrapped_keys: {},
  overlay: null,
  version: 1,
};

vi.mock('../state/store', () => ({
  useClusterState: () => cluster,
  useClusterStore: () => ({ status: 'open' }),
}));

vi.mock('../auth/AuthContext', () => ({
  useAuth: () => ({ isAdmin: true, identity: { subject: 'test', role: 'admin' }, loading: false }),
}));

// jsdom has no matchMedia, which the real hook probes for the "system" theme.
vi.mock('../theme', () => ({
  useTheme: () => ({ choice: 'system', setChoice: () => {} }),
}));

function renderLayout(initialPath: string) {
  return render(
    <MemoryRouter initialEntries={[initialPath]}>
      <ConfigDraftProvider>
        <Layout>
          <div>content</div>
        </Layout>
      </ConfigDraftProvider>
    </MemoryRouter>,
  );
}

describe('Layout sidebar', () => {
  it('lists every node as a submenu entry under Nodes when on a nodes route', () => {
    renderLayout('/nodes');
    const nav = screen.getByLabelText('Nodes sections');
    expect(nav).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'n1' })).toHaveAttribute('href', '/nodes/n1');
    expect(screen.getByRole('link', { name: 'n2' })).toHaveAttribute('href', '/nodes/n2');
  });

  it('does not expand the nodes submenu while on an unrelated page', () => {
    renderLayout('/stacks');
    expect(screen.queryByLabelText('Nodes sections')).not.toBeInTheDocument();
  });
});
