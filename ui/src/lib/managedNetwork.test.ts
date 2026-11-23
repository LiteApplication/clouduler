import { describe, expect, it } from 'vitest';
import type { ClusterConfig } from '../api/types';
import { managedNetworkError, managedNodeAddressError, nextFreeNodeAddress } from './managedNetwork';

function cfg(subnet: string, addrs: Record<string, string>, extra: Record<string, unknown> = {}): ClusterConfig {
  return {
    networks: {
      lan: { type: 'private', subnet: '192.168.1.0/24', domain: null },
      mgd: { type: 'private', subnet, domain: null, managed: true },
      ...extra,
    },
    nodes: Object.fromEntries(Object.entries(addrs).map(([n, a]) => [n, { network: { mgd: a } }])),
  } as unknown as ClusterConfig;
}

describe('managed networks', () => {
  it('autofills the lowest free host address', () => {
    expect(nextFreeNodeAddress(cfg('192.168.32.0/24', {}), 'mgd')).toBe('192.168.32.2');
    expect(nextFreeNodeAddress(cfg('192.168.32.0/24', { a: '192.168.32.2', b: '192.168.32.4' }), 'mgd')).toBe('192.168.32.3');
    expect(nextFreeNodeAddress(cfg('192.168.32.0/29', { a: '192.168.32.2', b: '192.168.32.3', c: '192.168.32.4', d: '192.168.32.5', e: '192.168.32.6' }), 'mgd')).toBeNull();
    expect(nextFreeNodeAddress(cfg('192.168.32.0/24', {}), 'lan')).toBeNull();
  });
  it('checks the subnet', () => {
    expect(managedNetworkError(cfg('192.168.32.0/24', {}), 'mgd')).toBeNull();
    expect(managedNetworkError(cfg('192.168.32.0/30', {}), 'mgd')).toMatch(/prefix length/);
    expect(managedNetworkError(cfg('10.50.0.0/15', {}), 'mgd')).toMatch(/prefix length/);
    expect(managedNetworkError(cfg('192.168.32.4/24', {}), 'mgd')).toMatch(/host bits/);
    expect(managedNetworkError(cfg('10.244.1.0/24', {}), 'mgd')).toMatch(/overlay pool/);
    expect(managedNetworkError(cfg('192.168.0.0/16', {}), 'mgd')).toMatch(/overlaps the subnet/);
    expect(managedNetworkError(cfg('fd00::/64', {}), 'mgd')).toMatch(/IPv4/);
  });
  it('checks node addresses', () => {
    const c = cfg('192.168.32.0/24', { a: '192.168.32.2', b: '192.168.32.2' });
    expect(managedNodeAddressError(c, 'a', 'mgd')).toMatch(/used by node b/);
    for (const [ip, re] of [['192.168.33.2', /outside/], ['192.168.32.0', /network or broadcast/], ['192.168.32.255', /network or broadcast/], ['192.168.32.1', /gateway/]] as const) {
      expect(managedNodeAddressError(cfg('192.168.32.0/24', { a: ip }), 'a', 'mgd')).toMatch(re);
    }
    expect(managedNodeAddressError(cfg('192.168.32.0/24', { a: '192.168.32.9' }), 'a', 'mgd')).toBeNull();
  });
  it('needs an IPv4 internal address on the node', () => {
    const c = cfg('192.168.32.0/24', { a: '192.168.32.9' }, { ts: { type: 'internal', subnet: 'fd00::/64', domain: null } });
    (c.nodes.a.network as Record<string, unknown>).ts = 'fd00::1';
    expect(managedNodeAddressError(c, 'a', 'mgd')).toMatch(/no IPv4 address on the internal network ts/);
    (c.nodes.a.network as Record<string, unknown>).ts = '10.10.0.1';
    expect(managedNodeAddressError(c, 'a', 'mgd')).toBeNull();
  });
});
