// Pure force simulation for the network graph. No DOM, no React, no
// Math.random: the same seed and the same calls give the same positions.

export interface LayoutNode {
  id: string;
  x: number;
  y: number;
  vx: number;
  vy: number;
  /** While set the node follows the pointer and ignores forces. */
  pinned: boolean;
}

export interface LayoutState {
  nodes: LayoutNode[];
  /** Simulation heat: scales forces, decays to a small floor so nodes keep drifting. */
  alpha: number;
  /** Accumulated time in seconds, drives the drift. */
  t: number;
}

export type LayoutEdge = readonly [string, string];
export interface Size {
  width: number;
  height: number;
}

/** Half the widest node and half the node height, plus a margin. */
export const PAD_X = 70;
export const PAD_Y = 40;
const ALPHA_FLOOR = 0.12;
const ALPHA_DECAY = 0.35; // per second
const REPULSION = 7000;
const SPRING_LEN = 170;
const SPRING_K = 0.9;
const CENTER_K = 0.1;
const DAMPING = 2.4; // per second
const DRIFT = 14;

function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** Initial state: nodes on a jittered circle, or at their saved positions. */
export function createLayout(
  ids: string[],
  size: Size,
  seed = 1,
  saved: Record<string, { x: number; y: number }> = {},
): LayoutState {
  const rnd = mulberry32(seed);
  const cx = size.width / 2;
  const cy = size.height / 2;
  const r = Math.min(size.width, size.height) * 0.3;
  const nodes = ids.map((id, i) => {
    const s = saved[id];
    const ang = (i / Math.max(ids.length, 1)) * Math.PI * 2 + rnd() * 0.4;
    return {
      id,
      x: s ? s.x : cx + Math.cos(ang) * r,
      y: s ? s.y : cy + Math.sin(ang) * r,
      vx: 0,
      vy: 0,
      pinned: false,
    };
  });
  return { nodes, alpha: 1, t: 0 };
}

/** Keeps existing nodes, adds new ones near the centre, drops removed ones. */
export function syncNodes(state: LayoutState, ids: string[], size: Size, seed = 1): LayoutState {
  const have = new Map(state.nodes.map((n) => [n.id, n]));
  const rnd = mulberry32(seed + state.nodes.length * 7919);
  const nodes = ids.map(
    (id) =>
      have.get(id) ?? {
        id,
        x: size.width / 2 + (rnd() - 0.5) * 80,
        y: size.height / 2 + (rnd() - 0.5) * 80,
        vx: 0,
        vy: 0,
        pinned: false,
      },
  );
  return { ...state, nodes, alpha: Math.max(state.alpha, 0.6) };
}

/** Raises the heat so a changed edge set re-settles. */
export function reheat(state: LayoutState, alpha = 0.7): void {
  state.alpha = Math.max(state.alpha, alpha);
}

export function clampToBounds(n: LayoutNode, size: Size, padX = PAD_X, padY = PAD_Y): void {
  const minX = Math.min(padX, size.width / 2);
  const maxX = Math.max(size.width - padX, size.width / 2);
  const minY = Math.min(padY, size.height / 2);
  const maxY = Math.max(size.height - padY, size.height / 2);
  if (n.x < minX) { n.x = minX; n.vx = Math.abs(n.vx) * 0.3; }
  if (n.x > maxX) { n.x = maxX; n.vx = -Math.abs(n.vx) * 0.3; }
  if (n.y < minY) { n.y = minY; n.vy = Math.abs(n.vy) * 0.3; }
  if (n.y > maxY) { n.y = maxY; n.vy = -Math.abs(n.vy) * 0.3; }
}

/** Advances the simulation by `dt` seconds (clamped), mutating `state`. */
export function step(state: LayoutState, edges: readonly LayoutEdge[], dtIn: number, size: Size): void {
  const dt = Math.min(Math.max(dtIn, 0), 0.05);
  if (dt === 0) return;
  const { nodes } = state;
  const index = new Map(nodes.map((n, i) => [n.id, i]));
  const ax = new Float64Array(nodes.length);
  const ay = new Float64Array(nodes.length);
  const a = state.alpha;
  const cx = size.width / 2;
  const cy = size.height / 2;

  for (let i = 0; i < nodes.length; i++) {
    for (let j = i + 1; j < nodes.length; j++) {
      let dx = nodes[i].x - nodes[j].x;
      let dy = nodes[i].y - nodes[j].y;
      let d2 = dx * dx + dy * dy;
      if (d2 < 1) {
        // Exactly coincident: separate along a fixed, index-derived axis.
        dx = Math.cos(i * 2.399);
        dy = Math.sin(i * 2.399);
        d2 = 1;
      }
      const d = Math.sqrt(d2);
      const f = (REPULSION * Math.max(a, 0.5)) / d;
      ax[i] += (dx / d) * f;
      ay[i] += (dy / d) * f;
      ax[j] -= (dx / d) * f;
      ay[j] -= (dy / d) * f;
    }
  }

  const seen = new Set<string>();
  for (const [s, t] of edges) {
    const key = s < t ? `${s}\0${t}` : `${t}\0${s}`;
    if (seen.has(key)) continue; // a mutual pair pulls once
    seen.add(key);
    const i = index.get(s);
    const j = index.get(t);
    if (i === undefined || j === undefined || i === j) continue;
    const dx = nodes[j].x - nodes[i].x;
    const dy = nodes[j].y - nodes[i].y;
    const d = Math.max(Math.hypot(dx, dy), 1);
    const f = SPRING_K * (d - SPRING_LEN) * Math.max(a, 0.5);
    ax[i] += (dx / d) * f;
    ay[i] += (dy / d) * f;
    ax[j] -= (dx / d) * f;
    ay[j] -= (dy / d) * f;
  }

  state.t += dt;
  for (let i = 0; i < nodes.length; i++) {
    const n = nodes[i];
    if (n.pinned) {
      n.vx = 0;
      n.vy = 0;
      continue;
    }
    ax[i] += (cx - n.x) * CENTER_K;
    ay[i] += (cy - n.y) * CENTER_K;
    // Gentle per-node wander so the graph never looks frozen.
    ax[i] += Math.sin(state.t * 0.9 + i * 1.7) * DRIFT;
    ay[i] += Math.cos(state.t * 0.7 + i * 2.3) * DRIFT;
    n.vx = (n.vx + ax[i] * dt) * Math.exp(-DAMPING * dt);
    n.vy = (n.vy + ay[i] * dt) * Math.exp(-DAMPING * dt);
    n.x += n.vx * dt;
    n.y += n.vy * dt;
    clampToBounds(n, size);
  }
  state.alpha = Math.max(ALPHA_FLOOR, state.alpha - ALPHA_DECAY * dt * state.alpha);
}

/** Runs `ticks` steps at 1/60 s, e.g. to settle a layout when motion is reduced. */
export function settle(state: LayoutState, edges: readonly LayoutEdge[], size: Size, ticks = 240): void {
  for (let i = 0; i < ticks; i++) step(state, edges, 1 / 60, size);
}
