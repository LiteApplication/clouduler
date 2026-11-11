import { describe, expect, it } from 'vitest';
import type { NetworkConfig } from '../../api/types';
import {
  defaultDnsNetworks,
  effectiveDnsNetworks,
  isLastServed,
  removeDnsNetwork,
  renameDnsNetwork,
  toggleDnsNetwork,
} from './dnsServerNetworks';

const nets = {
  int: { type: 'internal' },
  lan: { type: 'private' },
  pub: { type: 'public' },
  c1: { type: 'container', container: { proxy: true } },
  c2: { type: 'container', container: { proxy: false } },
} as unknown as Record<string, NetworkConfig>;

describe('dns server networks', () => {
  it('computes the default set', () => {
    expect(defaultDnsNetworks(nets)).toEqual(['c1', 'int', 'lan']);
    expect(effectiveDnsNetworks(nets, [])).toEqual(['c1', 'int', 'lan']);
    expect(effectiveDnsNetworks(nets, ['pub'])).toEqual(['pub']);
  });
  it('adding a public network writes the explicit list', () => {
    expect(toggleDnsNetwork(nets, [], 'pub', true)).toEqual(['c1', 'int', 'lan', 'pub']);
  });
  it('collapses to [] when equal to the default', () => {
    expect(toggleDnsNetwork(nets, ['c1', 'int', 'lan', 'pub'], 'pub', false)).toEqual([]);
  });
  it('turning one off from default writes the rest', () => {
    expect(toggleDnsNetwork(nets, [], 'lan', false)).toEqual(['c1', 'int']);
  });
  it('refuses to switch off the last served network', () => {
    expect(isLastServed(nets, ['pub'], 'pub')).toBe(true);
    expect(toggleDnsNetwork(nets, ['pub'], 'pub', false)).toBeNull();
  });
  it('rename and remove keep the list consistent', () => {
    expect(renameDnsNetwork(['a', 'b'], 'a', 'z')).toEqual(['z', 'b']);
    expect(removeDnsNetwork(['a', 'b'], 'a')).toEqual(['b']);
  });
});
