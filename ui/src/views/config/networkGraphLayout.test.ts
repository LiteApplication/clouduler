import { describe, expect, it } from 'vitest';
import { createLayout, PAD_X, PAD_Y, reheat, settle, step, type LayoutEdge } from './networkGraphLayout';

const size = { width: 700, height: 420 };
const ids = ['a', 'b', 'c', 'd'];

function run(edges: LayoutEdge[]) {
  const s = createLayout(ids, size, 3);
  settle(s, edges, size, 600);
  return s;
}
const dist = (s: ReturnType<typeof run>, a: string, b: string) => {
  const p = s.nodes.find((n) => n.id === a)!;
  const q = s.nodes.find((n) => n.id === b)!;
  return Math.hypot(p.x - q.x, p.y - q.y);
};

describe('network graph layout', () => {
  it('is deterministic for a given seed', () => {
    const e: LayoutEdge[] = [['a', 'b']];
    expect(run(e).nodes).toEqual(run(e).nodes);
  });

  it('keeps nodes within bounds, even when pushed outside', () => {
    const s = createLayout(ids, size, 1);
    s.nodes[0].x = -500;
    s.nodes[1].y = 9999;
    settle(s, [['a', 'b']], size, 300);
    for (const n of s.nodes) {
      expect(n.x).toBeGreaterThanOrEqual(PAD_X - 0.001);
      expect(n.x).toBeLessThanOrEqual(size.width - PAD_X + 0.001);
      expect(n.y).toBeGreaterThanOrEqual(PAD_Y - 0.001);
      expect(n.y).toBeLessThanOrEqual(size.height - PAD_Y + 0.001);
    }
  });

  it('pulls connected nodes closer than unconnected ones', () => {
    const s = run([['a', 'b'], ['b', 'a']]);
    expect(dist(s, 'a', 'b')).toBeLessThan(dist(s, 'c', 'd'));
  });

  it('does not move a pinned node and reheat raises alpha', () => {
    const s = createLayout(ids, size, 1);
    settle(s, [], size, 600);
    expect(s.alpha).toBeLessThan(0.5);
    reheat(s, 0.8);
    expect(s.alpha).toBeGreaterThanOrEqual(0.8);
    s.nodes[0].pinned = true;
    const { x, y } = s.nodes[0];
    step(s, [['a', 'b']], 1 / 60, size);
    expect([s.nodes[0].x, s.nodes[0].y]).toEqual([x, y]);
  });
});
