import { describe, expect, it } from 'vitest';
import type { Connectivity, PeerLink } from '../api/types';
import { classify, pairLinks } from './links';

const link = (peer: string, ok: boolean): PeerLink => ({ peer, raft: ok, api: ok, raft_ms: ok ? 1 : null });

describe('classify', () => {
  it('maps what each end sees to a status', () => {
    expect(classify(true, true)).toBe('healthy');
    expect(classify(false, false)).toBe('broken');
    expect(classify(true, false)).toBe('half');
    expect(classify(false, true)).toBe('half');
    expect(classify(true, undefined)).toBe('half');
    expect(classify(false, undefined)).toBe('broken');
    expect(classify(undefined, undefined)).toBe('unknown');
  });
});

describe('pairLinks', () => {
  it('shows an asymmetric break as half-broken and a silent node as unreported', () => {
    const c: Connectivity = {
      reports: [
        { node: 'a', reported: true, links: [link('b', true), link('c', true)] },
        { node: 'b', reported: true, links: [link('a', false), link('c', true)] },
        { node: 'c', reported: false, links: [] },
      ],
    };
    const by = Object.fromEntries(pairLinks(['c', 'b', 'a'], c).map((p) => [`${p.a}-${p.b}`, p.status]));
    expect(by).toEqual({ 'a-b': 'half', 'a-c': 'half', 'b-c': 'half' });
  });

  it('has no opinion without any report', () => {
    expect(pairLinks(['a', 'b'], null).map((p) => p.status)).toEqual(['unknown']);
  });

  it('treats a missing api as a degraded link', () => {
    const c: Connectivity = {
      reports: [
        { node: 'a', reported: true, links: [{ peer: 'b', raft: true, api: false, raft_ms: 2 }] },
        { node: 'b', reported: true, links: [{ peer: 'a', raft: true, api: false, raft_ms: 2 }] },
      ],
    };
    expect(pairLinks(['a', 'b'], c)[0].status).toBe('broken');
  });
});
