/**
 * Slug used for the fallback screenshot of a UiScreen: `${scenario}--${route}--${theme}`.
 * Plain module (no React) so the screenshot script can import it.
 * The route is lowercased, every run of characters outside [a-z0-9] becomes
 * one '-', leading/trailing '-' are trimmed, and '/' alone becomes 'root'.
 * Example: ('lan-3-nodes', '/stacks/web?x=1', 'dark') -> 'lan-3-nodes--stacks-web-x-1--dark'.
 */
export function sanitizeRoute(route: string): string {
  const s = route.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '');
  return s === '' ? 'root' : s;
}

export function uiScreenSlug(scenario: string, route: string, theme: 'light' | 'dark'): string {
  return `${scenario}--${sanitizeRoute(route)}--${theme}`;
}
