import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';
import { describe, expect, it, vi } from 'vitest';
import { Layout } from './Layout';
import type { ClusterState } from '../api/types';

const cluster: ClusterState = {
  nodes: {
    n1: { name: 'n1', role: 'manager', rpc_addr: '10.10.0.1:7373', drives: {}, availability: 'active', labels: {}, egress_networks: [], networks: [], open_ports: {}, arch: 'x86_64', binary_hash: '', node_pubkey: null, memory_total_bytes: 0 },
    n2: { name: 'n2', role: 'worker', rpc_addr: '10.10.0.2:7373', drives: {}, availability: 'active', labels: {}, egress_networks: [], networks: [], open_ports: {}, arch: 'x86_64', binary_hash: '', node_pubkey: null, memory_total_bytes: 0 },
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
  useClusterStore: () => ({ status: 'open' }),
}));

vi.mock('../state/instancesStore', () => ({
  useInstances: () => [
    { id: 'web.app.0', state: 'exited' },
    { id: 'web.db.0', state: 'running' },
    { id: 'media.jf.0', state: 'running' },
  ],
  failedStacks: (list: { id: string; state: string }[]) =>
    new Set(list.filter((i) => i.state === 'exited').map((i) => i.id.split('.')[0])),
}));

vi.mock('../api/client', () => ({
  api: {
    getConfiguration: () => Promise.resolve({ cluster: { name: 'homelab' }, networks: { lan: {}, vpn: {} } }),
    logout: () => Promise.resolve(),
  },
}));

vi.mock('../auth/AuthContext', () => ({
  useAuth: () => ({
    isAdmin: true,
    identity: { subject: 'test', username: 'test', role: 'admin' },
    loading: false,
    refresh: () => Promise.resolve(),
  }),
}));

// jsdom has no matchMedia, which the real hook probes for the "system" theme.
vi.mock('../theme', () => ({
  useTheme: () => ({ choice: 'system', setChoice: () => {} }),
}));

function renderLayout(initialPath: string) {
  return render(
    <MemoryRouter initialEntries={[initialPath]}>
      <Layout>
        <div>content</div>
      </Layout>
    </MemoryRouter>,
  );
}

describe('Layout rail', () => {
  it('lists the destinations and marks the current one', () => {
    renderLayout('/stacks');
    const nav = screen.getByRole('navigation', { name: 'Main navigation' });
    for (const label of ['Overview', 'Stacks', 'Nodes', 'Logs', 'Proxy', 'DNS', 'Storage', 'Updates', 'Settings']) {
      expect(within(nav).getByRole('button', { name: new RegExp(label) })).toBeInTheDocument();
    }
    expect(within(nav).getByRole('button', { name: /Stacks/ })).toHaveAttribute('aria-current', 'page');
    expect(within(nav).getByRole('button', { name: /Overview/ })).not.toHaveAttribute('aria-current');
  });

  it('badges Stacks with the number of failing stacks', () => {
    renderLayout('/');
    const nav = screen.getByRole('navigation', { name: 'Main navigation' });
    expect(within(nav).getByRole('button', { name: /Stacks/ })).toHaveTextContent('1');
  });

  it('keeps Instances and networks inside the collapsed More group', async () => {
    renderLayout('/');
    const nav = screen.getByRole('navigation', { name: 'Main navigation' });
    expect(within(nav).queryByRole('button', { name: 'Instances' })).not.toBeInTheDocument();
    await userEvent.click(within(nav).getByRole('button', { name: 'More' }));
    expect(within(nav).getByRole('button', { name: 'Instances' })).toBeInTheDocument();
    expect(await within(nav).findByRole('button', { name: 'lan' })).toBeInTheDocument();
  });

  it('opens the More group by itself on an instances route', () => {
    renderLayout('/instances/web.app.0');
    const nav = screen.getByRole('navigation', { name: 'Main navigation' });
    expect(within(nav).getByRole('button', { name: 'Instances' })).toHaveAttribute('aria-current', 'page');
  });

  it('has the five phone destinations in the bottom nav', () => {
    renderLayout('/nodes');
    const bottom = screen.getByRole('navigation', { name: 'Primary' });
    expect(within(bottom).getAllByRole('button').map((b) => b.textContent?.replace(/\d+$/, ''))).toEqual([
      'Overview',
      'Stacks',
      'Nodes',
      'Logs',
      'More',
    ]);
    expect(within(bottom).getByRole('button', { name: /Nodes/ })).toHaveAttribute('aria-current', 'page');
  });
});
