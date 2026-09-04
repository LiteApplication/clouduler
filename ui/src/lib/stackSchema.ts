// Lightweight, indentation-based context tracking for stack-file (compose +
// x-clouduler) yaml, used to drive CodeMirror autocomplete. This is not a
// real yaml parser: it just walks the lines above the cursor and tracks a
// stack of "key at indent N", which is enough to know which schema level the
// cursor sits at without pulling in a full yaml AST library.

import type { CompletionContext, CompletionResult } from '@codemirror/autocomplete';

export interface KnownNames {
  networks: string[];
  drives: string[];
  nodes: string[];
  instances: string[];
  secrets: string[];
  configs: string[];
}

export const emptyKnownNames: KnownNames = {
  networks: [],
  drives: [],
  nodes: [],
  instances: [],
  secrets: [],
  configs: [],
};

// Parent keys whose immediate map children are user-chosen names (a service
// name, a drive name under volume_sync, a declared config/secret name)
// rather than fixed schema keys — so the next path segment normalizes to `*`
// instead of being looked up literally.
const DYNAMIC_PARENTS = new Set(['services', 'volume_sync', 'configs', 'secrets']);

/** Ancestor key path (unnormalized) for the position at the start of `line`. */
function contextPath(doc: string, offsetInDoc: number): string[] {
  const before = doc.slice(0, offsetInDoc);
  const lines = before.split('\n');
  const curIndent = lines[lines.length - 1].match(/^ */)![0].length;
  const stack: { indent: number; key: string }[] = [];
  for (let i = 0; i < lines.length - 1; i++) {
    const raw = lines[i];
    if (!raw.trim() || raw.trim().startsWith('#')) continue;
    const indent = raw.match(/^ */)![0].length;
    const m = raw.trim().match(/^-?\s*([A-Za-z0-9_.-]+):/);
    if (!m) continue;
    while (stack.length && stack[stack.length - 1].indent >= indent) stack.pop();
    stack.push({ indent, key: m[1] });
  }
  while (stack.length && stack[stack.length - 1].indent >= curIndent) stack.pop();
  return stack.map((s) => s.key);
}

function normalizePath(path: string[]): string[] {
  return path.map((key, i) => (i > 0 && DYNAMIC_PARENTS.has(path[i - 1]) ? '*' : key));
}

/** Schema keys valid at a given normalized path (dot-joined, '' = root). */
const SCHEMA: Record<string, string[]> = {
  '': ['version', 'services', 'networks', 'volumes', 'configs', 'secrets', 'x-clouduler'],
  'services.*': [
    'image', 'hostname', 'command', 'entrypoint', 'environment', 'env_file', 'labels', 'volumes',
    'ports', 'networks', 'depends_on', 'healthcheck', 'restart', 'user', 'working_dir', 'cap_add',
    'cap_drop', 'sysctls', 'read_only', 'stop_grace_period', 'stop_signal', 'configs', 'secrets',
    'deploy', 'x-clouduler',
  ],
  'services.*.healthcheck': ['test', 'interval', 'timeout', 'start_period', 'retries', 'disable'],
  'services.*.deploy': ['replicas', 'mode', 'placement', 'resources', 'restart_policy', 'update_config'],
  'services.*.deploy.placement': ['constraints'],
  'services.*.deploy.resources': ['limits', 'reservations'],
  'services.*.deploy.resources.limits': ['cpus', 'memory'],
  'services.*.deploy.resources.reservations': ['cpus', 'memory'],
  'services.*.deploy.restart_policy': ['condition', 'delay', 'max_attempts', 'window'],
  'services.*.deploy.update_config': ['parallelism', 'delay', 'order', 'failure_action'],
  'services.*.x-clouduler': ['proxy', 'expose', 'volume_sync', 'auto_update', 'node', 'backup'],
  'services.*.x-clouduler.proxy': [
    'host', 'network', 'port', 'path_prefix', 'tls', 'middlewares', 'sticky', 'max_body_size',
    'websockets', 'preserve_host', 'response_timeout',
  ],
  'services.*.x-clouduler.expose': ['port', 'published', 'protocol', 'network', 'proxy_protocol'],
  'services.*.x-clouduler.volume_sync.*': ['interval', 'enabled'],
  'services.*.x-clouduler.backup': ['stop_before'],
  'services.*.configs': ['source', 'target', 'mode', 'uid', 'gid', 'env'],
  'services.*.secrets': ['source', 'target', 'mode', 'uid', 'gid', 'env'],
  'x-clouduler': ['auto_update', 'node', 'volume_sync', 'backup'],
  'x-clouduler.volume_sync.*': ['interval', 'enabled'],
  'x-clouduler.backup': ['stop_before'],
  'configs.*': ['file'],
  'secrets.*': ['file'],
};

function valueNamesFor(key: string, path: string[], known: KnownNames): string[] | null {
  if (key === 'network') return known.networks;
  if (key === 'node') return known.nodes;
  if (key === 'source') {
    const container = path[path.length - 1];
    if (container === 'volumes') return known.drives.map((d) => `${d}/`);
    if (container === 'secrets') return known.secrets;
    if (container === 'configs') return known.configs;
  }
  return null;
}

function listValueNamesFor(path: string[], known: KnownNames): string[] | null {
  const container = path[path.length - 1];
  if (container === 'networks' && path[0] === 'services') return known.networks;
  return null;
}

/** CodeMirror `autocompletion()` source combining schema keys and live cluster
 *  object names, based on a heuristic indentation-tracked yaml context. */
export function stackSpecCompletionSource(getKnown: () => KnownNames) {
  return (context: CompletionContext): CompletionResult | null => {
    const state = context.state;
    const line = state.doc.lineAt(context.pos);
    const col = context.pos - line.from;
    const before = line.text.slice(0, col);
    const known = getKnown();

    const kvMatch = before.match(/^(\s*)(?:-\s*)?([A-Za-z0-9_.-]+):\s*(\S*)$/);
    const listMatch = !kvMatch && before.match(/^(\s*)-\s*(\S*)$/);

    if (kvMatch) {
      const path = normalizePath(contextPath(state.doc.toString(), line.from));
      const names = valueNamesFor(kvMatch[2], path, known);
      if (!names || names.length === 0) return null;
      const wordLen = kvMatch[3].length;
      return {
        from: context.pos - wordLen,
        options: names.map((n) => ({ label: n, type: 'text' })),
        validFor: /^\S*$/,
      };
    }

    if (listMatch) {
      const path = normalizePath(contextPath(state.doc.toString(), line.from));
      const names = listValueNamesFor(path, known);
      if (!names || names.length === 0) return null;
      const wordLen = listMatch[2].length;
      return {
        from: context.pos - wordLen,
        options: names.map((n) => ({ label: n, type: 'text' })),
        validFor: /^\S*$/,
      };
    }

    const word = context.matchBefore(/[A-Za-z0-9_-]*/);
    if (!word || (word.from === word.to && !context.explicit)) return null;
    const path = normalizePath(contextPath(state.doc.toString(), word.from));
    const keys = SCHEMA[path.join('.')];
    if (!keys || keys.length === 0) return null;
    return {
      from: word.from,
      options: keys.map((k) => ({ label: k, type: 'property', apply: `${k}: ` })),
      validFor: /^[A-Za-z0-9_-]*$/,
    };
  };
}

export type ObjectKind = 'network' | 'drive' | 'node' | 'instance';

export function objectRouteFor(kind: ObjectKind, name: string): string {
  switch (kind) {
    case 'network':
      return `/networks/${encodeURIComponent(name)}`;
    case 'drive':
      return `/drives/${encodeURIComponent(name)}`;
    case 'node':
      return `/nodes/${encodeURIComponent(name)}`;
    case 'instance':
      return `/instances/${encodeURIComponent(name)}`;
  }
}

/** name -> kind lookup for the ctrl+click decoration pass. Instances are
 *  checked last so a network/drive/node sharing a substring of an instance
 *  id (rare, but instance ids are "stack.service.index") still resolves to
 *  the more specific kind first via a plain map's last-write-wins. */
export function buildObjectIndex(known: KnownNames): Map<string, ObjectKind> {
  const map = new Map<string, ObjectKind>();
  for (const n of known.instances) map.set(n, 'instance');
  for (const n of known.nodes) map.set(n, 'node');
  for (const n of known.drives) map.set(n, 'drive');
  for (const n of known.networks) map.set(n, 'network');
  return map;
}
