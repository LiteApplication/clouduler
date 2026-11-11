// Floating graph of the cluster's networks. An edge A -> B means B is listed in
// networks[A].access. Public networks are implicitly reachable and drawn dim.

import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type PointerEvent as RPointerEvent } from 'react';
import type { NetworkConfig } from '../../api/types';
import { useConfirm } from '../../ds';
import { canHaveAccess, explicitEdges, implicitPublicEdges, isAccessTarget } from './networkAccess';
import { createLayout, settle, step, syncNodes, reheat, clampToBounds, type LayoutState, type Size } from './networkGraphLayout';
import './networkGraph.css';

const POS_KEY = 'cl.netgraph.pos';
const FALLBACK: Size = { width: 720, height: 420 };
const NODE_H = 48;

function loadPositions(): Record<string, { x: number; y: number }> {
  try {
    const raw = localStorage.getItem(POS_KEY);
    return raw ? (JSON.parse(raw) as Record<string, { x: number; y: number }>) : {};
  } catch {
    return {};
  }
}

function savePositions(state: LayoutState) {
  try {
    const out: Record<string, { x: number; y: number }> = {};
    for (const n of state.nodes) out[n.id] = { x: Math.round(n.x), y: Math.round(n.y) };
    localStorage.setItem(POS_KEY, JSON.stringify(out));
  } catch {
    /* positions are a convenience only */
  }
}

function reducedMotion(): boolean {
  try {
    return typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches;
  } catch {
    return false;
  }
}

const nodeWidth = (name: string) => Math.max(104, Math.min(190, name.length * 8 + 34));

/** Point where a ray from the node centre toward (tx, ty) leaves its box. */
function clip(cx: number, cy: number, hw: number, hh: number, tx: number, ty: number, gap: number): [number, number] {
  const dx = tx - cx;
  const dy = ty - cy;
  if (dx === 0 && dy === 0) return [cx, cy];
  const t = Math.min(dx === 0 ? Infinity : hw / Math.abs(dx), dy === 0 ? Infinity : hh / Math.abs(dy));
  const len = Math.hypot(dx, dy);
  const k = t + gap / len;
  return [cx + dx * k, cy + dy * k];
}

interface Props {
  networks: Record<string, NetworkConfig>;
  editable: boolean;
  onOpen: (name: string) => void;
  onAddEdge: (from: string, to: string) => void;
  onRemoveEdge: (from: string, to: string) => void;
}

interface Drag {
  id: string;
  mode: 'move' | 'link';
  startX: number;
  startY: number;
  moved: boolean;
}

export function NetworkGraph({ networks, editable, onOpen, onAddEdge, onRemoveEdge }: Props) {
  const confirm = useConfirm();
  const wrapRef = useRef<HTMLDivElement>(null);
  const svgRef = useRef<SVGSVGElement>(null);
  const [size, setSize] = useState<Size>(FALLBACK);
  const [, setFrame] = useState(0);
  const [rubber, setRubber] = useState<{ from: string; x: number; y: number } | null>(null);
  const drag = useRef<Drag | null>(null);
  const suppressClick = useRef(false);

  const ids = useMemo(() => Object.keys(networks).sort(), [networks]);
  const explicit = useMemo(() => explicitEdges(networks), [networks]);
  const implicit = useMemo(() => implicitPublicEdges(networks), [networks]);
  const edgeKey = explicit.map((e) => e.join('>')).join('|');

  const sizeRef = useRef(size);
  sizeRef.current = size;
  const edgesRef = useRef(explicit);
  edgesRef.current = explicit;
  const stateRef = useRef<LayoutState>(null as unknown as LayoutState);
  if (!stateRef.current) {
    stateRef.current = createLayout(ids, size, 7, loadPositions());
  }

  // Responsive width.
  useEffect(() => {
    const el = wrapRef.current;
    if (!el || typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver((entries) => {
      const w = Math.round(entries[0]?.contentRect.width ?? 0);
      if (w > 0) setSize({ width: w, height: w < 520 ? 380 : 440 });
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // Node set or edge set changed: keep positions, re-settle.
  const idKey = ids.join('|');
  useEffect(() => {
    stateRef.current = syncNodes(stateRef.current, ids, sizeRef.current, 7);
    if (reducedMotion()) {
      settle(stateRef.current, edgesRef.current, sizeRef.current);
      setFrame((f) => f + 1);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [idKey]);
  useEffect(() => {
    reheat(stateRef.current);
    if (reducedMotion()) {
      settle(stateRef.current, edgesRef.current, sizeRef.current);
      setFrame((f) => f + 1);
    }
  }, [edgeKey]);
  useEffect(() => {
    for (const n of stateRef.current.nodes) clampToBounds(n, size);
  }, [size]);

  // Animation loop; stops on unmount and is skipped when motion is reduced.
  useEffect(() => {
    if (typeof requestAnimationFrame !== 'function') return;
    let raf = 0;
    let last = performance.now();
    let alive = true;
    const loop = (now: number) => {
      if (!alive) return;
      step(stateRef.current, edgesRef.current, (now - last) / 1000, sizeRef.current);
      last = now;
      setFrame((f) => f + 1);
      raf = requestAnimationFrame(loop);
    };
    if (reducedMotion()) {
      settle(stateRef.current, edgesRef.current, sizeRef.current);
      setFrame((f) => f + 1);
    } else {
      raf = requestAnimationFrame(loop);
    }
    return () => {
      alive = false;
      cancelAnimationFrame(raf);
    };
  }, []);

  const nodes = stateRef.current.nodes.filter((n) => n.id in networks);
  const pos = new Map(nodes.map((n) => [n.id, n]));
  const dims = (id: string) => ({ hw: nodeWidth(id) / 2, hh: NODE_H / 2 });

  const toSvg = (e: { clientX: number; clientY: number }) => {
    const r = svgRef.current?.getBoundingClientRect();
    return { x: e.clientX - (r?.left ?? 0), y: e.clientY - (r?.top ?? 0) };
  };
  const nodeAt = (x: number, y: number, exclude: string): string | null => {
    for (const n of nodes) {
      if (n.id === exclude) continue;
      const { hw, hh } = dims(n.id);
      if (Math.abs(n.x - x) <= hw + 6 && Math.abs(n.y - y) <= hh + 6) return n.id;
    }
    return null;
  };

  const begin = (e: RPointerEvent, id: string, link: boolean) => {
    if (e.button !== 0 && e.pointerType === 'mouse') return;
    const p = toSvg(e);
    drag.current = { id, mode: link && editable ? 'link' : 'move', startX: p.x, startY: p.y, moved: false };
    try {
      svgRef.current?.setPointerCapture?.(e.pointerId);
    } catch {
      /* not capturable */
    }
    if (link && editable) {
      e.stopPropagation();
      setRubber({ from: id, x: p.x, y: p.y });
    } else {
      const n = pos.get(id);
      if (n) n.pinned = true;
    }
  };

  const move = (e: RPointerEvent) => {
    const d = drag.current;
    if (!d) return;
    const p = toSvg(e);
    if (!d.moved && Math.hypot(p.x - d.startX, p.y - d.startY) > 4) d.moved = true;
    if (d.mode === 'link') {
      setRubber({ from: d.id, x: p.x, y: p.y });
    } else if (d.moved) {
      const n = pos.get(d.id);
      if (n) {
        n.x = p.x;
        n.y = p.y;
        clampToBounds(n, sizeRef.current);
        reheat(stateRef.current, 0.35);
        if (reducedMotion()) setFrame((f) => f + 1);
      }
    }
  };

  const end = (e: RPointerEvent) => {
    const d = drag.current;
    drag.current = null;
    if (!d) return;
    try {
      svgRef.current?.releasePointerCapture?.(e.pointerId);
    } catch {
      /* ignore */
    }
    const n = pos.get(d.id);
    if (n) n.pinned = false;
    if (d.moved) suppressClick.current = true;
    if (d.mode === 'link') {
      setRubber(null);
      const p = toSvg(e);
      const to = nodeAt(p.x, p.y, d.id);
      if (d.moved && to) tryAdd(d.id, to);
    } else if (d.moved) {
      savePositions(stateRef.current);
    }
  };

  const tryAdd = (from: string, to: string) => {
    const a = networks[from];
    const b = networks[to];
    if (!editable || !a || !b || from === to) return;
    if (!canHaveAccess(a.type) || !isAccessTarget(b.type)) return;
    if (explicit.some(([x, y]) => x === from && y === to)) return;
    onAddEdge(from, to);
  };

  const open = (id: string) => {
    if (suppressClick.current) {
      suppressClick.current = false;
      return;
    }
    onOpen(id);
  };

  const removeEdge = async (from: string, to: string) => {
    if (!editable) return;
    const ok = await confirm({
      title: `Remove access from ${from} to ${to}?`,
      body: `${from} will no longer be able to reach ${to}.`,
      confirmLabel: 'Remove',
      confirmVariant: 'danger',
    });
    if (ok) onRemoveEdge(from, to);
  };

  const onKey = (e: KeyboardEvent, id: string) => {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      onOpen(id);
    }
  };

  /** Geometry of an edge: curved, bowing to the right of its direction. */
  const geometry = (a: string, b: string, bow: number) => {
    const pa = pos.get(a);
    const pb = pos.get(b);
    if (!pa || !pb) return null;
    const mx = (pa.x + pb.x) / 2;
    const my = (pa.y + pb.y) / 2;
    const dx = pb.x - pa.x;
    const dy = pb.y - pa.y;
    const len = Math.max(Math.hypot(dx, dy), 1);
    const cx = mx + (-dy / len) * bow;
    const cy = my + (dx / len) * bow;
    const da = dims(a);
    const db = dims(b);
    const [x1, y1] = clip(pa.x, pa.y, da.hw, da.hh, cx, cy, 3);
    const [x2, y2] = clip(pb.x, pb.y, db.hw, db.hh, cx, cy, 5);
    return { d: `M${x1.toFixed(1)},${y1.toFixed(1)} Q${cx.toFixed(1)},${cy.toFixed(1)} ${x2.toFixed(1)},${y2.toFixed(1)}` };
  };

  const hasEdge = (a: string, b: string) => explicit.some(([x, y]) => x === a && y === b);

  return (
    <div className="cl-ng" ref={wrapRef}>
      <svg
        ref={svgRef}
        className="cl-ng__svg"
        width="100%"
        height={size.height}
        viewBox={`0 0 ${size.width} ${size.height}`}
        role="group"
        aria-label="Network access graph"
        onPointerMove={move}
        onPointerUp={end}
        onPointerCancel={end}
      >
        <defs>
          <marker id="cl-ng-arrow" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="9" markerHeight="9" orient="auto" markerUnits="userSpaceOnUse">
            <path className="cl-ng__head" d="M0,1 L9,5 L0,9 z" />
          </marker>
          <marker id="cl-ng-arrow-dim" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="7" markerHeight="7" orient="auto" markerUnits="userSpaceOnUse">
            <path className="cl-ng__head cl-ng__head--dim" d="M0,1 L9,5 L0,9 z" />
          </marker>
        </defs>

        {implicit.map(([a, b]) => {
          const g = geometry(a, b, 12);
          return g && <path key={`i-${a}-${b}`} className="cl-ng__edge cl-ng__edge--implicit" d={g.d} markerEnd="url(#cl-ng-arrow-dim)" />;
        })}

        {explicit.map(([a, b]) => {
          const mutual = hasEdge(b, a);
          const g = geometry(a, b, mutual ? 30 : 14);
          if (!g) return null;
          return (
            <g key={`e-${a}-${b}`} className="cl-ng__edgeg">
              <path className="cl-ng__edge cl-ng__edge--flow" d={g.d} markerEnd="url(#cl-ng-arrow)" />
              {editable && (
                <path
                  className="cl-ng__hit"
                  d={g.d}
                  role="button"
                  tabIndex={0}
                  data-testid={`edge-${a}-${b}`}
                  aria-label={`Remove access from ${a} to ${b}`}
                  onClick={() => removeEdge(a, b)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault();
                      void removeEdge(a, b);
                    }
                  }}
                />
              )}
            </g>
          );
        })}

        {rubber && pos.get(rubber.from) && (
          <line className="cl-ng__rubber" x1={pos.get(rubber.from)!.x} y1={pos.get(rubber.from)!.y} x2={rubber.x} y2={rubber.y} />
        )}

        {nodes.map((n) => {
          const net = networks[n.id];
          const w = nodeWidth(n.id);
          const src = canHaveAccess(net.type);
          const unrestricted = src && net.access == null;
          return (
            <g
              key={n.id}
              className={`cl-ng__node cl-ng__node--${net.type}`}
              transform={`translate(${n.x.toFixed(1)},${n.y.toFixed(1)})`}
              role="button"
              tabIndex={0}
              aria-label={`${n.id} network settings`}
              onPointerDown={(e) => begin(e, n.id, e.shiftKey)}
              onClick={() => open(n.id)}
              onKeyDown={(e) => onKey(e, n.id)}
            >
              <rect className="cl-ng__box" x={-w / 2} y={-NODE_H / 2} width={w} height={NODE_H} rx={NODE_H / 2} />
              <text className="cl-ng__name" y={-3} textAnchor="middle">
                {n.id}
              </text>
              <text className="cl-ng__kind" y={13} textAnchor="middle">
                {net.type}
              </text>
              {unrestricted && (
                <g className="cl-ng__badge" transform={`translate(0,${NODE_H / 2 + 13})`}>
                  <rect x={-34} y={-8} width={68} height={15} rx={7.5} />
                  <text textAnchor="middle" y={3}>
                    unrestricted
                  </text>
                </g>
              )}
              {editable && src && (
                <circle
                  className="cl-ng__handle"
                  cx={w / 2}
                  cy={0}
                  r={7}
                  data-testid={`handle-${n.id}`}
                  onPointerDown={(e) => begin(e, n.id, true)}
                  onClick={(e) => e.stopPropagation()}
                >
                  <title>{`Drag onto a network to let ${n.id} reach it`}</title>
                </circle>
              )}
            </g>
          );
        })}
      </svg>

      <ul className="cl-ng__legend" aria-label="Legend">
        <li>
          <svg width="34" height="10" aria-hidden="true"><line className="cl-ng__edge cl-ng__edge--flow" x1="1" y1="5" x2="33" y2="5" /></svg>
          Allowed (animated)
        </li>
        <li>
          <svg width="40" height="10" aria-hidden="true">
            <path className="cl-ng__head" d="M0,5 L8,1 L8,9 z" />
            <line className="cl-ng__edge cl-ng__edge--solid" x1="8" y1="5" x2="32" y2="5" />
            <path className="cl-ng__head" d="M40,5 L32,1 L32,9 z" />
          </svg>
          Mutual
        </li>
        <li>
          <svg width="34" height="10" aria-hidden="true"><line className="cl-ng__edge cl-ng__edge--implicit" x1="1" y1="5" x2="33" y2="5" /></svg>
          Implicit (public)
        </li>
        <li>
          <span className="cl-ng__legendbadge">unrestricted</span>
          No host firewall
        </li>
      </ul>
      {editable && <p className="cl-ng__hint">Drag a node to move it, drag the dot on its right edge onto another network to allow access, click a line to remove it.</p>}
    </div>
  );
}
