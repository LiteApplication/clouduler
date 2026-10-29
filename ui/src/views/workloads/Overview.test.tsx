import { render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { Overview } from './Overview';
import type { ClusterState } from '../../api/types';

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

vi.mock('../../state/store', () => ({
  useClusterState: () => cluster,
}));

// Exactly the body GET /api/status returns, field for field. Written out here
// rather than built from the ClusterStatus type on purpose: TypeScript types
// are erased at runtime, so a type that disagrees with the server compiles
// perfectly and fails only in the browser. This literal is the wire contract,
// and it is what makes the assertions below mean anything.
//
// The bug this guards: the ui read `quorum`, the api has always sent
// `has_quorum`. `!undefined` is `true`, so a healthy single-node cluster
// showed "Quorum lost" permanently.
const healthyStatus = {
  node: 'n1',
  is_leader: true,
  has_quorum: true,
  leader: 'n1',
  voters: 1,
  reachable: 1,
  managers_total: 1,
  managers_reachable: 1,
  version: '0.1.0',
  state_version: 42,
};

const status = vi.fn();
const listInstances = vi.fn();
vi.mock('../../api/client', () => ({
  api: {
    status: () => status(),
    listInstances: () => listInstances(),
    listUpdates: () => Promise.resolve([]),
    nodeMetrics: () => Promise.resolve({}),
  },
}));

function renderOverview() {
  return render(
    <MemoryRouter>
      <Overview />
    </MemoryRouter>,
  );
}

describe('Overview header', () => {
  beforeEach(() => {
    listInstances.mockResolvedValue([]);
  });

  it('offers deploy stack but not add node, which lives on the Nodes page', async () => {
    status.mockResolvedValue(healthyStatus);
    renderOverview();
    expect(screen.getByRole('button', { name: /deploy stack/i })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /add node/i })).not.toBeInTheDocument();
    await waitFor(() => expect(screen.getByText(/cluster healthy/i)).toBeInTheDocument());
  });
});

describe('Overview quorum banner', () => {
  beforeEach(() => {
    listInstances.mockResolvedValue([]);
  });

  it('reports a healthy single-node cluster as healthy', async () => {
    status.mockResolvedValue(healthyStatus);
    renderOverview();
    await waitFor(() => expect(screen.getByText(/cluster healthy/i)).toBeInTheDocument());
    expect(screen.queryByText(/quorum lost/i)).not.toBeInTheDocument();
  });

  it('reports lost quorum when the api says so', async () => {
    status.mockResolvedValue({ ...healthyStatus, has_quorum: false, leader: null });
    renderOverview();
    await waitFor(() => expect(screen.getByText(/quorum lost/i)).toBeInTheDocument());
    expect(screen.queryByText(/cluster healthy/i)).not.toBeInTheDocument();
  });

  /// Before the status call resolves there is nothing to complain about, and
  /// flashing a red banner on every page load is worse than saying nothing.
  it('does not claim lost quorum before the status call resolves', () => {
    status.mockReturnValue(new Promise(() => {}));
    renderOverview();
    expect(screen.queryByText(/quorum lost/i)).not.toBeInTheDocument();
  });
});
