import type { Connectivity, NodeReport } from '../api/types';

/** How the two ends of a node pair see their link.
 *  healthy: both see it. broken: nobody who can speak sees it.
 *  half: one side sees it, the other does not (or cannot report at all).
 *  unknown: neither side reported. */
export type LinkStatus = 'healthy' | 'broken' | 'half' | 'unknown';

export interface PairLink {
  a: string;
  b: string;
  status: LinkStatus;
  /** Does a see b / does b see a; undefined when that side has no report. */
  aSeesB: boolean | undefined;
  bSeesA: boolean | undefined;
}

function sees(report: NodeReport | undefined, peer: string): boolean | undefined {
  if (!report?.reported) return undefined;
  const link = report.links.find((l) => l.peer === peer);
  // A reporting node that lists no entry for a peer does not know it.
  return link ? link.raft && link.api : false;
}

export function classify(aSeesB: boolean | undefined, bSeesA: boolean | undefined): LinkStatus {
  if (aSeesB === undefined && bSeesA === undefined) return 'unknown';
  if (aSeesB === true && bSeesA === true) return 'healthy';
  if (aSeesB === true || bSeesA === true) return 'half';
  return 'broken';
}

/** One entry per unordered pair of nodes, in a stable order. */
export function pairLinks(names: string[], connectivity: Connectivity | null): PairLink[] {
  const reports = new Map((connectivity?.reports ?? []).map((r) => [r.node, r]));
  const sorted = [...names].sort();
  const pairs: PairLink[] = [];
  for (let i = 0; i < sorted.length; i++) {
    for (let j = i + 1; j < sorted.length; j++) {
      const a = sorted[i];
      const b = sorted[j];
      const aSeesB = sees(reports.get(a), b);
      const bSeesA = sees(reports.get(b), a);
      pairs.push({ a, b, status: classify(aSeesB, bSeesA), aSeesB, bSeesA });
    }
  }
  return pairs;
}

/** What one directed cell of the matrix says: `from` as it reports `to`.
 *  self: the diagonal. ok: raft and api both connect (rtt in ms when known).
 *  down: the raft connect failed. api-down: raft works, the api port does not.
 *  no-report: `from` could not be asked, or it did not list `to`. */
export type CellState =
  | { kind: 'self' }
  | { kind: 'ok'; ms: number | null }
  | { kind: 'down' }
  | { kind: 'api-down' }
  | { kind: 'no-report' };

export function cellState(connectivity: Connectivity | null, from: string, to: string): CellState {
  if (from === to) return { kind: 'self' };
  const report = connectivity?.reports.find((r) => r.node === from);
  if (!report?.reported) return { kind: 'no-report' };
  const link = report.links.find((l) => l.peer === to);
  if (!link) return { kind: 'no-report' };
  if (!link.raft) return { kind: 'down' };
  if (!link.api) return { kind: 'api-down' };
  return { kind: 'ok', ms: link.raft_ms ?? null };
}
