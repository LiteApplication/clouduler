// Release tags are `v0.9.4`, the running version is `0.9.4`.
export const bare = (v: string) => v.replace(/^v/, '');

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
