import { describe, expect, it } from 'vitest';
import { diffConfig } from './configDiff';

describe('config diff', () => {
  it('finds no differences between identical objects', () => {
    const cfg = { cluster: { name: 'home' }, web: { port: 443 } };
    expect(diffConfig(cfg, cfg)).toEqual([]);
  });

  it('reports a changed leaf value with its full path', () => {
    const before = { cluster: { name: 'home' } };
    const after = { cluster: { name: 'lab' } };
    expect(diffConfig(before, after)).toEqual([{ path: 'cluster.name', before: 'home', after: 'lab' }]);
  });

  it('reports an added key', () => {
    const before = { networks: {} } as Record<string, unknown>;
    const after = { networks: { wan: { type: 'public' } } };
    const entries = diffConfig(before, after);
    expect(entries).toEqual([{ path: 'networks.wan', before: '(none)', after: '{"type":"public"}' }]);
  });

  it('reports a removed key', () => {
    const before = { networks: { wan: { type: 'public' } } };
    const after = { networks: {} };
    const entries = diffConfig(before, after);
    expect(entries).toEqual([{ path: 'networks.wan', before: '{"type":"public"}', after: '(none)' }]);
  });

  it('reports array changes as a single entry rather than per-element', () => {
    const before = { zones: ['a.com'] };
    const after = { zones: ['a.com', 'b.com'] };
    expect(diffConfig(before, after)).toEqual([{ path: 'zones', before: '["a.com"]', after: '["a.com","b.com"]' }]);
  });

  it('walks nested node maps down to the changed field', () => {
    const before = { nodes: { n1: { rpc_port: 7373 } } };
    const after = { nodes: { n1: { rpc_port: 7000 } } };
    expect(diffConfig(before, after)).toEqual([{ path: 'nodes.n1.rpc_port', before: '7373', after: '7000' }]);
  });
});
