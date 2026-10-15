import { useEffect, useState } from 'react';
import { api } from '../api/client';
import type { Connectivity } from '../api/types';
import { pairLinks, type LinkStatus } from '../lib/links';

const COLORS: Record<LinkStatus, string> = {
  healthy: 'var(--good)',
  broken: 'var(--bad)',
  half: 'var(--warn)',
  unknown: 'var(--muted, #888)',
};

const LABEL: Record<LinkStatus, string> = {
  healthy: 'both ends see each other',
  broken: 'neither end can reach the other',
  half: 'one-sided: only one end sees the other',
  unknown: 'no node reported on this link',
};

const SIZE = 360;
const R_NODE = 22;

/** Cluster nodes as vertices, the links between them as edges, drawn from
 *  each node's own report so an asymmetric break shows as half-broken. */
export function NodeGraph({ names, pollMs = 10000 }: { names: string[]; pollMs?: number }) {
  const [data, setData] = useState<Connectivity | null>(null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    let cancelled = false;
    const load = () =>
      api
        .nodeConnectivity()
        .then((c) => {
          if (!cancelled) {
            setData(c);
            setFailed(false);
          }
        })
        .catch(() => {
          if (!cancelled) setFailed(true);
        });
    void load();
    const t = setInterval(load, pollMs);
    return () => {
      cancelled = true;
      clearInterval(t);
    };
  }, [pollMs]);

  const sorted = [...names].sort();
  const pos = new Map(
    sorted.map((n, i) => {
      const angle = (2 * Math.PI * i) / Math.max(sorted.length, 1) - Math.PI / 2;
      const r = SIZE / 2 - R_NODE - 24;
      return [n, { x: SIZE / 2 + r * Math.cos(angle), y: SIZE / 2 + r * Math.sin(angle) }] as const;
    }),
  );
  const reported = new Map((data?.reports ?? []).map((r) => [r.node, r.reported]));
  const pairs = pairLinks(sorted, data);

  return (
    <div className="node-graph">
      <svg viewBox={`0 0 ${SIZE} ${SIZE}`} role="img" aria-label="Node connectivity graph" style={{ maxWidth: 480, width: '100%' }}>
        {pairs.map((p) => {
          const a = pos.get(p.a)!;
          const b = pos.get(p.b)!;
          const dashed = p.status !== 'healthy';
          return (
            <line
              key={`${p.a}-${p.b}`}
              data-testid={`edge-${p.a}-${p.b}`}
              data-status={p.status}
              x1={a.x}
              y1={a.y}
              x2={b.x}
              y2={b.y}
              stroke={COLORS[p.status]}
              strokeWidth={2.5}
              strokeDasharray={dashed ? '6 5' : undefined}
            >
              <title>{`${p.a} ↔ ${p.b}: ${LABEL[p.status]}`}</title>
            </line>
          );
        })}
        {sorted.map((n) => {
          const { x, y } = pos.get(n)!;
          const silent = data !== null && reported.get(n) !== true;
          return (
            <g key={n} data-testid={`vertex-${n}`} data-reported={!silent}>
              <circle cx={x} cy={y} r={R_NODE} fill="var(--surface, #fff)" stroke={silent ? COLORS.broken : 'var(--text, #333)'} strokeWidth={2} strokeDasharray={silent ? '3 3' : undefined} />
              <text x={x} y={y + 4} textAnchor="middle" fontSize={11} fill="var(--text, #333)">
                {n.length > 6 ? `${n.slice(0, 5)}…` : n}
              </text>
              <title>{silent ? `${n}: no report (could not be asked)` : n}</title>
            </g>
          );
        })}
      </svg>
      <p className="muted" style={{ fontSize: 12 }}>
        {failed && 'Could not load connectivity. '}
        <span style={{ color: COLORS.healthy }}>━ healthy</span> · <span style={{ color: COLORS.half }}>╌ one-sided</span> ·{' '}
        <span style={{ color: COLORS.broken }}>╌ broken</span> · dashed circle: node gave no report
      </p>
    </div>
  );
}
