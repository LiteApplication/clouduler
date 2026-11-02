// Path-pattern matching shared by the http routes and the stream routes.

export interface Matched<T extends { pattern: string }> {
  route: T;
  params: Record<string, string>;
}

/** Pick the route whose pattern matches, preferring the most literal one so
 *  `/api/nodes/metrics` wins over `/api/nodes/:name`. */
export function matchRoute<T extends { pattern: string }>(routes: T[], path: string): Matched<T> | null {
  const segs = path.split('/').filter(Boolean);
  let best: { route: T; params: Record<string, string>; score: number } | null = null;
  for (const route of routes) {
    const pat = route.pattern.split('/').filter(Boolean);
    if (pat.length !== segs.length) continue;
    const params: Record<string, string> = {};
    let score = 0;
    let ok = true;
    for (let i = 0; i < pat.length; i++) {
      if (pat[i].startsWith(':')) {
        try {
          params[pat[i].slice(1)] = decodeURIComponent(segs[i]);
        } catch {
          params[pat[i].slice(1)] = segs[i];
        }
      } else if (pat[i] === segs[i]) score += 1;
      else {
        ok = false;
        break;
      }
    }
    if (ok && (!best || score > best.score)) best = { route, params, score };
  }
  return best && { route: best.route, params: best.params };
}
