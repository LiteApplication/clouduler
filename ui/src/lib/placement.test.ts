import { describe, expect, it } from 'vitest';
import { buildPlacementRows, nodeEligibility, parseConstraint, type PlacementContext } from './placement';
import type { ClusterState, NodeState, StackRequirements } from '../api/types';

const node = (name: string, extra: Partial<NodeState> = {}): NodeState => ({
  name, role: 'worker', rpc_addr: '10.0.0.1:7373', drives: {}, availability: 'active', labels: {},
  egress_networks: [], networks: ['internal'], open_ports: {}, arch: '', binary_hash: '', node_pubkey: null,
  memory_total_bytes: 0, ...extra,
});

const cluster = {
  nodes: { a: node('a', { drives: { fast: '/x' } }), b: node('b', { availability: 'drain' }) },
  unreachable: [],
  configs: {},
  secrets: {},
  wrapped_keys: { a: {}, b: {} },
} as unknown as ClusterState;

const ctx: PlacementContext = { servedDrives: new Set(), natNetworks: new Set(), driveHolders: {}, portGatedNetworks: new Set() };

const reqs = (extra: Partial<StackRequirements>): StackRequirements => ({
  networks: [], drives: [], configs: [], secrets: [], file_configs: [], file_secrets: [], hosts: [],
  pinned_node: null, placement_constraints: [], bound_ports: [], whole_drive_mounts: [], ...extra,
});

describe('placement rows', () => {
  it('parses swarm constraints with != taking precedence', () => {
    expect(parseConstraint('node.labels.fast!=true')).toEqual({ key: 'node.labels.fast', equal: false, value: 'true' });
    expect(parseConstraint('nonsense')).toBeNull();
  });

  it('always checks reachable and not-drained, and flags a missing drive', () => {
    const rows = buildPlacementRows(reqs({ drives: ['fast'] }), cluster, ctx);
    expect(rows.map((r) => r.label)).toEqual(['reachable', 'not drained', 'drive: fast']);
    const e = nodeEligibility(rows, cluster);
    expect(e.find((n) => n.name === 'a')?.eligible).toBe(true);
    const b = e.find((n) => n.name === 'b')!;
    expect(b.eligible).toBe(false);
    expect(b.reasons).toEqual(['drain', 'no such drive']);
  });

  it('marks a missing secret as creatable', () => {
    const rows = buildPlacementRows(reqs({ secrets: ['pw'] }), cluster, ctx);
    const row = rows.find((r) => r.label === 'secret: pw')!;
    expect(row).toMatchObject({ missing: true, kind: 'secret', name: 'pw' });
  });
});
