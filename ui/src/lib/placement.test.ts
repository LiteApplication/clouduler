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

  describe('gpu and device rows', () => {
    const gpus = (extra: Partial<NonNullable<NodeState['gpus']>> = {}): NodeState['gpus'] => ({
      nvidia_devices: [], nvidia: [], nvidia_runtime: false, nvidia_hook: null, render_nodes: [], dev_paths: [], ...extra,
    });
    const gpuCluster = {
      ...cluster,
      nodes: {
        none: node('none'),
        one: node('one', { gpus: gpus({ nvidia_devices: ['/dev/nvidia0'], nvidia: [{ index: 0, uuid: 'GPU-x', name: 'T4' }], nvidia_runtime: true }) }),
        noruntime: node('noruntime', { gpus: gpus({ nvidia_devices: ['/dev/nvidia0', '/dev/nvidia1'] }) }),
        igpu: node('igpu', { gpus: gpus({ dev_paths: ['/dev/dri', '/dev/dri/renderD128'] }) }),
      },
    } as unknown as ClusterState;
    const eligible = (r: Partial<StackRequirements>) =>
      nodeEligibility(buildPlacementRows(reqs(r), gpuCluster, ctx), gpuCluster)
        .filter((n) => n.eligible)
        .map((n) => n.name);

    it('needs enough NVIDIA GPUs and the docker runtime', () => {
      expect(eligible({ gpu_requests: [{ count: 1, device_ids: [] }] })).toEqual(['one']);
      expect(eligible({ gpu_requests: [{ count: 2, device_ids: [] }] })).toEqual([]);
      expect(eligible({ gpu_requests: [{ count: 1, device_ids: ['GPU-x'] }] })).toEqual(['one']);
      expect(eligible({ gpu_requests: [{ count: 1, device_ids: ['5'] }] })).toEqual([]);
    });

    it('accepts the toolkit hook in place of a registered runtime', () => {
      const hooked = {
        ...gpuCluster,
        nodes: { h: node('h', { gpus: gpus({ nvidia_devices: ['/dev/nvidia0'], nvidia_hook: '/usr/bin/nvidia-container-cli' }) }) },
      } as unknown as ClusterState;
      const rows = buildPlacementRows(reqs({ gpu_requests: [{ count: 1, device_ids: [] }] }), hooked, ctx);
      expect(nodeEligibility(rows, hooked)[0].eligible).toBe(true);
    });

    it('says why a node falls short', () => {
      const rows = buildPlacementRows(reqs({ gpu_requests: [{ count: 1, device_ids: [] }] }), gpuCluster, ctx);
      const why = Object.fromEntries(nodeEligibility(rows, gpuCluster).map((n) => [n.name, n.reasons]));
      expect(why.none).toEqual(['no NVIDIA GPU']);
      expect(why.noruntime).toEqual(['no NVIDIA container toolkit']);
    });

    it('needs host devices to exist, unless the node is pinned', () => {
      expect(eligible({ host_devices: ['/dev/dri'] })).toEqual(['igpu']);
      expect(eligible({ host_devices: ['/dev/dri'], pinned_node: 'none' })).toEqual(['none']);
      expect(eligible({ host_devices: ['/dev/dri'], placement_constraints: ['node.hostname==none'] })).toEqual(['none']);
    });

    it('lets an explicit pin override a node that reported no GPU', () => {
      expect(eligible({ gpu_requests: [{ count: 1, device_ids: [] }], placement_constraints: ['node.hostname==none'] })).toEqual(['none']);
    });
  });
});
