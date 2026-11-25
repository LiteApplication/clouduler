// Release tags are `v0.9.4`, the running version is `0.9.4`.
export const bare = (v: string) => v.replace(/^v/, '');

const SEMVER = /^v?(\d+)\.(\d+)\.(\d+)$/;

/**
 * The releases to install, in order, to get from `running` to the newest one:
 * the last patch of every (major, minor) series that has something newer than
 * `running`, oldest series first. Tags that are not plain `vX.Y.Z` are ignored.
 */
export function planUpdatePath(running: string, tags: string[]): string[] {
  if (!SEMVER.test(bare(running))) return [];
  const best = new Map<string, { v: number[]; tag: string }>();
  for (const tag of tags) {
    const m = SEMVER.exec(tag);
    if (!m) continue;
    const v = [Number(m[1]), Number(m[2]), Number(m[3])];
    if (!isNewer(tag, running)) continue;
    const key = `${v[0]}.${v[1]}`;
    const prev = best.get(key);
    if (!prev || v[2] > prev.v[2]) best.set(key, { v, tag });
  }
  return [...best.values()]
    .sort((a, b) => a.v[0] - b.v[0] || a.v[1] - b.v[1])
    .map((e) => e.tag);
}

// Dotted numeric compare; a missing or non-numeric part counts as 0.
export function isNewer(tag: string, version: string): boolean {
  const parts = (v: string) => bare(v).split(/[.-]/).map((p) => parseInt(p, 10) || 0);
  const a = parts(tag);
  const b = parts(version);
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    const d = (a[i] ?? 0) - (b[i] ?? 0);
    if (d !== 0) return d > 0;
  }
  return false;
}
