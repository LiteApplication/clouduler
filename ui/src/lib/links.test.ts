import { describe, expect, it } from 'vitest';
import type { Connectivity, PeerLink } from '../api/types';
import { cellState, classify, linkHistory, pairLinks } from './links';

const link = (peer: string, ok: boolean): PeerLink => ({
  peer,
  raft: ok,
  api: ok,
  drive: ok,
  raft_ms: ok ? 1 : null,
  state: ok ? 'ok' : 'down',
  failures: 0,
  history: [],
});

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
        { node: 'a', reported: true, links: [{ ...link('b', true), api: false }] },
        { node: 'b', reported: true, links: [{ ...link('a', true), api: false }] },
      ],
    };
    expect(pairLinks(['a', 'b'], c)[0].status).toBe('broken');
  });
});

describe('cellState with disk and leader verdicts', () => {
  const conn = {
    reports: [
      { node: 'a', reported: true, links: [link('b', true)] },
      { node: 'b', reported: true, links: [], disk_ms: 4200 },
    ],
  };
  it('flags a peer whose disk is too slow to follow raft', () => {
    expect(cellState(conn, 'a', 'b')).toEqual({ kind: 'slow-disk', diskMs: 4200 });
  });
  it('flags a peer the leader has marked unreachable', () => {
    const ok = { reports: [{ node: 'a', reported: true, links: [link('b', true)] }] };
    expect(cellState(ok, 'a', 'b', ['b'])).toEqual({ kind: 'flagged' });
    expect(cellState(ok, 'a', 'b')).toEqual({ kind: 'ok', ms: expect.anything() });
  });
  it('flags a link whose drive-sync port is closed while raft and api work', () => {
    const c = { reports: [{ node: 'a', reported: true, links: [{ ...link('b', true), drive: false }] }] };
    expect(cellState(c, 'a', 'b')).toEqual({ kind: 'drive-down' });
    expect(pairLinks(['a', 'b'], c)[0].aSeesB).toBe(false);
  });
  it('reads a link that answers now but keeps failing as flapping and lists the history', () => {
    const sample = (at: number, up: boolean) => ({ at, raft: up, api: up, drive: up, raft_ms: up ? 1 : null });
    const history = [sample(1, true), sample(2, false), sample(3, true), sample(4, false), sample(5, true)];
    const c = { reports: [{ node: 'a', reported: true, links: [{ ...link('b', true), state: 'flapping' as const, failures: 2, history }] }] };
    expect(cellState(c, 'a', 'b')).toEqual({ kind: 'flapping', failures: 2, window: 5 });
    const h = linkHistory(c, 'a', 'b');
    expect(h).toMatchObject({ failures: 2, window: 5 });
    expect(h?.detail.split('\n')).toHaveLength(5);
    expect(linkHistory(c, 'a', 'zzz')).toBeNull();
    expect(linkHistory(c, 'b', 'a')).toBeNull();
  });
});
