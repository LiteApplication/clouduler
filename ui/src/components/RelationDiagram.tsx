// A bipartite schema: one column of "hub" nodes (networks, drives) on the
// left, one column of "member" nodes (cluster nodes) on the right, with
// SVG lines connecting the two wherever an edge says they relate. Position
// is measured off the real DOM (chips reflow in normal document flow), so
// it stays correct across screen sizes and item counts without a layout
// algorithm.

import { useLayoutEffect, useRef, useState, type ReactNode } from 'react';
import type { ChartTone } from './Donut';

export interface RelationNode {
  id: string;
  label: string;
  sub?: string;
  icon?: ReactNode;
  tone?: ChartTone;
}

export interface RelationEdge {
  from: string;
  to: string;
  tone?: ChartTone;
  dashed?: boolean;
}

const TONE_VAR: Record<ChartTone, string> = {
  good: 'var(--good)',
  warn: 'var(--warn)',
  bad: 'var(--bad)',
  neutral: 'var(--border-strong)',
  primary: 'var(--primary)',
  secondary: 'var(--secondary)',
  accent: 'var(--accent)',
};

interface Line {
  x1: number;
  y1: number;
  x2: number;
  y2: number;
  color: string;
  dashed?: boolean;
}

export function RelationDiagram({
  leftTitle,
  rightTitle,
  left,
  right,
  edges,
}: {
  leftTitle: string;
  rightTitle: string;
  left: RelationNode[];
  right: RelationNode[];
  edges: RelationEdge[];
}) {
  const containerRef = useRef<HTMLDivElement>(null);
  const chipRefs = useRef(new Map<string, HTMLDivElement>());
  const [lines, setLines] = useState<Line[]>([]);
  const [size, setSize] = useState({ w: 0, h: 0 });

  useLayoutEffect(() => {
    const measure = () => {
      const container = containerRef.current;
      if (!container) return;
      const box = container.getBoundingClientRect();
      const next: Line[] = [];
      for (const e of edges) {
        const fromEl = chipRefs.current.get(`l:${e.from}`);
        const toEl = chipRefs.current.get(`r:${e.to}`);
        if (!fromEl || !toEl) continue;
        const fr = fromEl.getBoundingClientRect();
        const tr = toEl.getBoundingClientRect();
        next.push({
          x1: fr.right - box.left,
          y1: fr.top + fr.height / 2 - box.top,
          x2: tr.left - box.left,
          y2: tr.top + tr.height / 2 - box.top,
          color: TONE_VAR[e.tone ?? 'neutral'],
          dashed: e.dashed,
        });
      }
      setLines(next);
      setSize({ w: box.width, h: box.height });
    };

    measure();
    const ro = new ResizeObserver(measure);
    if (containerRef.current) ro.observe(containerRef.current);
    window.addEventListener('resize', measure);
    return () => {
      ro.disconnect();
      window.removeEventListener('resize', measure);
    };
  }, [edges, left, right]);

  return (
    <div className="relation-diagram" ref={containerRef}>
      <svg className="relation-lines" width={size.w} height={size.h}>
        {lines.map((l, i) => {
          const midX = (l.x1 + l.x2) / 2;
          return (
            <path
              key={i}
              d={`M ${l.x1} ${l.y1} C ${midX} ${l.y1}, ${midX} ${l.y2}, ${l.x2} ${l.y2}`}
              fill="none"
              stroke={l.color}
              strokeWidth={1.5}
              strokeDasharray={l.dashed ? '4 4' : undefined}
              opacity={0.7}
            />
          );
        })}
      </svg>

      <div className="relation-col">
        <div className="relation-col-title">{leftTitle}</div>
        {left.map((n) => (
          <div
            key={n.id}
            ref={(el) => {
              if (el) chipRefs.current.set(`l:${n.id}`, el);
              else chipRefs.current.delete(`l:${n.id}`);
            }}
            className="relation-chip"
          >
            {n.icon}
            <span>
              <span className="relation-chip-label">{n.label}</span>
              {n.sub && <span className="relation-chip-sub">{n.sub}</span>}
            </span>
          </div>
        ))}
        {left.length === 0 && <p className="faint">nothing to show</p>}
      </div>

      <div className="relation-col relation-col-right">
        <div className="relation-col-title">{rightTitle}</div>
        {right.map((n) => (
          <div
            key={n.id}
            ref={(el) => {
              if (el) chipRefs.current.set(`r:${n.id}`, el);
              else chipRefs.current.delete(`r:${n.id}`);
            }}
            className={`relation-chip${n.tone ? ` relation-chip-${n.tone}` : ''}`}
          >
            {n.icon}
            <span>
              <span className="relation-chip-label">{n.label}</span>
              {n.sub && <span className="relation-chip-sub">{n.sub}</span>}
            </span>
          </div>
        ))}
        {right.length === 0 && <p className="faint">nothing to show</p>}
      </div>
    </div>
  );
}
