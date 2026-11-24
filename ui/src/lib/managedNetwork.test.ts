import { describe, expect, it } from 'vitest';
import type { ClusterConfig } from '../api/types';
import { addressedNodesOn, managedNetworkError, managedNodeAddresses, managedNodeAddressError, managedPortalError, managedReserved, nodeAddressOn } from './managedNetwork';

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
  it('allocates stable automatic node addresses from the top of the subnet', () => {
    const c = cfg('192.168.32.0/24', {});
    (c.nodes as Record<string, unknown>).a = { network: { mgd: null } };
    (c.nodes as Record<string, unknown>).b = { network: { mgd: null } };
    (c.nodes as Record<string, unknown>).c = { network: { mgd: '192.168.32.254' } };
    (c.nodes as Record<string, unknown>).d = { network: {} };
    const all = managedNodeAddresses(c, 'mgd');
    expect([...all.keys()].sort()).toEqual(['a', 'b', 'c']);
    expect(all.get('c')).toBe('192.168.32.254');
    const ips = [...all.values()];
    expect(new Set(ips).size).toBe(3);
    for (const ip of ips) expect(ip).not.toMatch(/\.(0|1|255)$/);
    expect(nodeAddressOn(c, 'a', 'mgd')).toBe(all.get('a'));
    expect(nodeAddressOn(c, 'd', 'mgd')).toBeNull();
    expect(managedNodeAddresses(c, 'lan').size).toBe(0);
    // Same config, same answer; automatic addresses are reserved.
    expect(managedNodeAddresses(c, 'mgd')).toEqual(all);
    expect(managedReserved(c, 'mgd').has(192 * 16777216 + 168 * 65536 + 32 * 256 + 254)).toBe(true);
    expect(managedNodeAddressError(c, 'a', 'mgd')).toBeNull();
    (c.nodes as Record<string, unknown>).e = { network: { mgd: '' } };
    expect(managedNodeAddressError(c, 'e', 'mgd')).toMatch(/tick automatic/);
  });
  it('checks open ports and proxy', () => {
    const p = (extra: Record<string, unknown>) => {
      const c = cfg('192.168.32.0/24', {});
      (c.networks as Record<string, unknown>).mgd = { type: 'private', subnet: '192.168.32.0/24', domain: null, managed: true, ...extra };
      return managedNetworkError(c, 'mgd');
    };
    const port = { port: 22, protocol: 'tcp', stack: 'git', service: 'ssh' };
    expect(p({ open_ports: [port], proxy: true })).toBeNull();
    expect(p({ open_ports: [port, { ...port, target_port: 2 }] })).toMatch(/more than once/);
    expect(p({ open_ports: [{ ...port, port: 0 }] })).toMatch(/port 0/);
    expect(p({ open_ports: [{ ...port, service: '' }] })).toMatch(/stack and service/);
    expect(p({ proxy: true, open_ports: [{ ...port, port: 443 }] })).toMatch(/reverse proxy/);
    expect(p({ open_ports: [{ ...port, port: 443 }] })).toBeNull();
  });
  it('lets the dashboard and login sit on a managed network a node lists', () => {
    const c = cfg('192.168.32.0/24', { a: '192.168.32.2' });
    (c.nodes as Record<string, unknown>).b = { network: { mgd: null } };
    expect(addressedNodesOn(c, 'mgd')).toEqual(['a', 'b']);
    expect(managedPortalError(c, 'mgd', 'the login page')).toBeNull();
    expect(managedPortalError(c, 'lan', 'the login page')).toBeNull();
    const none = cfg('192.168.32.0/24', {});
    expect(managedPortalError(none, 'mgd', 'the dashboard')).toMatch(/no node lists it/);
    expect(managedPortalError(none, 'lan', 'the dashboard')).toBeNull();
  });
});
