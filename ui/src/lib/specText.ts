// Small text helpers over a stack spec: finding the line a validation path
// points at, and a line diff for "changes since last deploy".

/** 1-based line of the key a dotted path (`services.web.image`) names, walking
 *  down the yaml indentation. Falls back to the deepest key that did match,
 *  or null when not even the first one is found. */
export function lineForPath(spec: string, path: string): number | null {
  if (!path) return null;
  const lines = spec.split('\n');
  const keys = path.split(/[./]/).filter(Boolean).map((k) => k.replace(/\[\d+\]$/, ''));
  let from = 0;
  let indent = -1;
  let found: number | null = null;
  for (const key of keys) {
    let hit = -1;
    for (let i = from; i < lines.length; i++) {
      const m = /^(\s*)(?:-\s+)?["']?([^:"']+)["']?\s*:/.exec(lines[i]);
      if (!m) continue;
      if (indent >= 0 && m[1].length <= indent) break;
      if (m[2].trim() === key && (indent < 0 || m[1].length > indent)) {
        hit = i;
        indent = m[1].length;
        break;
      }
    }
    if (hit < 0) break;
    found = hit + 1;
    from = hit + 1;
  }
  return found;
}

export interface DiffLine {
  kind: 'add' | 'del' | 'ctx';
  text: string;
}

/** Line diff by longest common subsequence, trimmed to `context` lines around
 *  each change. Specs are small, so the quadratic table is fine. */
export function diffLines(before: string, after: string, context = 2): DiffLine[] {
  const a = before.split('\n');
  const b = after.split('\n');
  const dp: number[][] = Array.from({ length: a.length + 1 }, () => new Array(b.length + 1).fill(0));
  for (let i = a.length - 1; i >= 0; i--) {
    for (let j = b.length - 1; j >= 0; j--) {
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const all: DiffLine[] = [];
  let i = 0;
  let j = 0;
  while (i < a.length && j < b.length) {
    if (a[i] === b[j]) {
      all.push({ kind: 'ctx', text: a[i] });
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      all.push({ kind: 'del', text: a[i++] });
    } else {
      all.push({ kind: 'add', text: b[j++] });
    }
  }
  while (i < a.length) all.push({ kind: 'del', text: a[i++] });
  while (j < b.length) all.push({ kind: 'add', text: b[j++] });

  const keep = new Array(all.length).fill(false);
  all.forEach((l, k) => {
    if (l.kind === 'ctx') return;
    for (let x = Math.max(0, k - context); x <= Math.min(all.length - 1, k + context); x++) keep[x] = true;
  });
  return all.filter((_, k) => keep[k]);
}
