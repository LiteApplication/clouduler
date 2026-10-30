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
  /** "drive/volume" ids, from `ClusterState.volumes`. */
  volumes: string[];
  /** Every `image:` value seen across every stack currently in the cluster
   *  (repo, or repo:tag when one is pinned), for image/tag autocomplete. */
  images: string[];
}

export const emptyKnownNames: KnownNames = {
  networks: [],
  drives: [],
  nodes: [],
  instances: [],
  secrets: [],
  configs: [],
  volumes: [],
  images: [],
};

/** Scans one or more stack spec yaml texts for `image:` lines. Not a real
 *  yaml parser — just enough to pool "images already used somewhere in this
 *  cluster" for completion, including images that aren't in this file yet. */
export function collectImages(specs: string[]): string[] {
  const names = new Set<string>();
  const re = /^[ \t]*image:[ \t]*(\S+)[ \t]*$/gm;
  for (const spec of specs) {
    let m: RegExpExecArray | null;
    while ((m = re.exec(spec))) names.add(m[1].replace(/^["']|["']$/g, ''));
  }
  return Array.from(names).sort();
}

// Parent keys whose immediate map children are user-chosen names (a service
// name, a drive name under volume_sync, a declared config/secret name)
// rather than fixed schema keys — so the next path segment normalizes to `*`
// instead of being looked up literally.
const DYNAMIC_PARENTS = new Set(['services', 'volume_sync', 'configs', 'secrets', 'actions']);

/** A line's nesting column: right after its `- ` marker when it's a list
 *  item, otherwise its plain leading-whitespace length. Used both to record
 *  where an ancestor key sits and to measure how deep the cursor's own line
 *  is, so the two are directly comparable. */
function lineIndent(raw: string): number {
  const dash = raw.match(/^(\s*)-\s+/);
  return dash ? dash[0].length : raw.match(/^ */)![0].length;
}

/** Ancestor key path (unnormalized) for a position `curIndent` columns deep
 *  on the line starting at `lineFrom`.
 *
 *  A list item's first key sits at the dash's own column (e.g. `- host:` is
 *  2 columns left of `host`), while its sibling keys on later lines sit at
 *  the column *after* the dash (where `host`'s text starts). Comparing those
 *  two raw indents directly means a list item's first key never gets popped
 *  by its own siblings, so it wrongly stays on the stack as their "parent"
 *  and every key after the first in a `- foo: 1\n  bar: 2` item resolves to
 *  the wrong schema path. `lineIndent` normalizes both to the same column so
 *  they pop each other correctly. */
function contextPath(
  doc: string,
  lineFrom: number,
  curIndent: number,
  onKey?: (path: string[]) => void,
): string[] {
  const lines = doc.slice(0, lineFrom).split('\n');
  const stack: { indent: number; key: string }[] = [];
  for (const raw of lines) {
    if (!raw.trim() || raw.trim().startsWith('#')) continue;
    const dash = raw.match(/^(\s*)-\s+/);
    const rest = dash ? raw.slice(dash[0].length) : raw.trimStart();
    const indent = lineIndent(raw);
    const m = rest.match(/^([A-Za-z0-9_.][A-Za-z0-9_. -]*):(?:\s|$)/);
    if (!m) continue;
    while (stack.length && stack[stack.length - 1].indent >= indent) stack.pop();
    stack.push({ indent, key: m[1] });
    onKey?.(stack.map((s) => s.key));
  }
  while (stack.length && stack[stack.length - 1].indent >= curIndent) stack.pop();
  return stack.map((s) => s.key);
}

/** Names of the actions defined on the service whose block the cursor is in.
 *  Action names are free-form labels, so they are read from the document. */
function actionNamesFor(doc: string, rawPath: string[]): string[] {
  if (rawPath[0] !== 'services' || rawPath.length < 2) return [];
  const prefix = [rawPath[0], rawPath[1], 'x-clouduler', 'actions'];
  const names: string[] = [];
  contextPath(doc, doc.length, 0, (path) => {
    if (path.length === prefix.length + 1 && prefix.every((k, i) => path[i] === k)) {
      names.push(path[path.length - 1]);
    }
  });
  return names;
}

function normalizePath(path: string[]): string[] {
  return path.map((key, i) => (i > 0 && DYNAMIC_PARENTS.has(path[i - 1]) ? '*' : key));
}

/** The immediate children of a top-level `key:` map/list in `doc`, in
 *  document order — e.g. `topLevelChildKeys(doc, 'services')` lists every
 *  service defined in the file currently being edited, whether or not it's
 *  been deployed yet. Same-file-aware complement to the cluster-wide
 *  `KnownNames`. */
function topLevelChildKeys(doc: string, parentKey: string): string[] {
  const names: string[] = [];
  let collecting = false;
  let childIndent: number | null = null;
  for (const raw of doc.split('\n')) {
    if (!raw.trim() || raw.trim().startsWith('#')) continue;
    const indent = raw.match(/^ */)![0].length;
    const m = raw.trim().match(/^([A-Za-z0-9_.-]+):/);
    if (indent === 0) {
      collecting = m !== null && m[1] === parentKey;
      childIndent = null;
      continue;
    }
    if (!collecting) continue;
    if (childIndent === null) childIndent = indent;
    if (indent === childIndent && m) names.push(m[1]);
  }
  return names;
}

function dedupe(values: string[]): string[] {
  return Array.from(new Set(values));
}

/** `image:` values already written elsewhere in the document being edited —
 *  reusing an image another service in the same stack already pins is the
 *  most common case (a web + worker pair on the same image), so it's worth
 *  surfacing even before the stack is saved and shows up in `known.images`. */
function localImages(doc: string): string[] {
  return collectImages([doc]);
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
  'services.*.volumes': ['type', 'source', 'target', 'read_only', 'tmpfs'],
  'services.*.volumes.tmpfs': ['size'],
  'services.*.deploy': ['replicas', 'mode', 'placement', 'resources', 'restart_policy', 'update_config'],
  'services.*.deploy.placement': ['constraints'],
  'services.*.deploy.resources': ['limits', 'reservations'],
  'services.*.deploy.resources.limits': ['cpus', 'memory'],
  'services.*.deploy.resources.reservations': ['cpus', 'memory'],
  'services.*.deploy.restart_policy': ['condition', 'delay', 'max_attempts', 'window'],
  'services.*.deploy.update_config': ['parallelism', 'delay', 'order', 'failure_action'],
  'services.*.x-clouduler': [
    'proxy', 'expose', 'volume_sync', 'auto_update', 'node', 'docker_socket', 'colocate', 'backup',
    'actions',
  ],
  'services.*.x-clouduler.proxy': [
    'host', 'network', 'port', 'path_prefix', 'tls', 'middlewares', 'sticky', 'max_body_size',
    'websockets', 'preserve_host', 'response_timeout',
  ],
  'services.*.x-clouduler.proxy.middlewares.basic_auth': ['users', 'realm'],
  'services.*.x-clouduler.proxy.middlewares.headers': ['request', 'response', 'remove_request', 'remove_response'],
  'services.*.x-clouduler.proxy.middlewares.cors': ['origins', 'methods', 'headers', 'allow_credentials', 'max_age'],
  'services.*.x-clouduler.proxy.middlewares.forward_auth': ['address', 'auth_response_headers', 'timeout'],
  'services.*.x-clouduler.proxy.middlewares.redirect_regex': ['regex', 'replacement', 'permanent'],
  'services.*.x-clouduler.expose': ['port', 'published', 'protocol', 'network', 'proxy_protocol'],
  'services.*.x-clouduler.volume_sync.*': ['interval', 'enabled', 'before', 'after'],
  'services.*.x-clouduler.docker_socket': ['target', 'read_only', 'mode'],
  'services.*.x-clouduler.backup': ['stop_before'],
  'services.*.x-clouduler.actions.*': ['cmd', 'user', 'timeout', 'confirm', 'show_in_ui'],
  'services.*.configs': ['source', 'target', 'mode', 'uid', 'gid', 'env'],
  'services.*.secrets': ['source', 'target', 'mode', 'uid', 'gid', 'env'],
  'x-clouduler': ['auto_update', 'node', 'volume_sync', 'backup'],
  'x-clouduler.volume_sync.*': ['interval', 'enabled'],
  'x-clouduler.backup': ['stop_before'],
  'configs.*': ['file'],
  'secrets.*': ['file'],
};

/** Map key names that are always plain booleans, wherever they appear —
 *  none of them collide with a differently-typed field of the same name. */
const BOOLEAN_KEYS = new Set([
  'tls', 'sticky', 'websockets', 'preserve_host', 'proxy_protocol', 'allow_credentials', 'permanent',
  'stop_before', 'auto_update', 'read_only', 'enabled', 'disable', 'confirm', 'show_in_ui',
]);

const BARE_MIDDLEWARES = ['redirect_https', 'strip_prefix', 'authenticated'];
const CONFIGURED_MIDDLEWARES = ['basic_auth', 'headers', 'cors', 'forward_auth', 'redirect_regex'];

const RESTART_VALUES = ['no', 'always', 'on-failure', 'unless-stopped'];

function valueNamesFor(
  key: string,
  path: string[],
  known: KnownNames,
  doc: string,
  rawPath: string[] = [],
): string[] | null {
  const container = path[path.length - 1];

  if ((key === 'before' || key === 'after') && path.join('.') === 'services.*.x-clouduler.volume_sync.*') {
    return actionNamesFor(doc, rawPath);
  }

  if (key === 'network') return dedupe([...topLevelChildKeys(doc, 'networks'), ...known.networks]);
  if (key === 'node') return known.nodes;
  if (key === 'image') return dedupe([...localImages(doc), ...known.images]);
  if (key === 'restart') return RESTART_VALUES;
  if (key === 'protocol') return ['tcp', 'udp'];
  if (key === 'mode' && container === 'deploy') return ['replicated', 'global'];
  if (key === 'condition' && container === 'restart_policy') return ['none', 'on-failure', 'any'];
  if (key === 'order' && container === 'update_config') return ['start-first', 'stop-first'];
  if (key === 'failure_action' && container === 'update_config') return ['continue', 'pause', 'rollback'];
  if (BOOLEAN_KEYS.has(key)) return ['true', 'false'];

  if (key === 'source') {
    if (container === 'volumes') return known.volumes.length ? known.volumes : known.drives.map((d) => `${d}/`);
    if (container === 'secrets') return dedupe([...known.secrets, ...topLevelChildKeys(doc, 'secrets')]);
    if (container === 'configs') return dedupe([...known.configs, ...topLevelChildKeys(doc, 'configs')]);
  }
  return null;
}

function listValueNamesFor(path: string[], known: KnownNames, doc: string): string[] | null {
  const container = path[path.length - 1];
  if (path[0] !== 'services') return null;
  if (container === 'networks') return dedupe([...topLevelChildKeys(doc, 'networks'), ...known.networks]);
  if (container === 'volumes') {
    return known.volumes.length ? known.volumes.map((v) => `${v}:`) : known.drives.map((d) => `${d}/`);
  }
  if (container === 'depends_on' || container === 'colocate') return topLevelChildKeys(doc, 'services');
  if (container === 'middlewares') return [...BARE_MIDDLEWARES, ...CONFIGURED_MIDDLEWARES.map((n) => `${n}:`)];
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
    const doc = state.doc.toString();

    const kvMatch = before.match(/^(\s*)(?:-\s*)?([A-Za-z0-9_.-]+):\s*(\S*)$/);
    const listMatch = !kvMatch && before.match(/^(\s*)-\s*(\S*)$/);

    if (kvMatch) {
      const rawPath = contextPath(doc, line.from, lineIndent(before));
      const path = normalizePath(rawPath);
      const names = valueNamesFor(kvMatch[2], path, known, doc, rawPath);
      if (!names || names.length === 0) return null;
      const wordLen = kvMatch[3].length;
      return {
        from: context.pos - wordLen,
        options: names.map((n) => ({ label: n, type: 'text' })),
        validFor: /^\S*$/,
      };
    }

    if (listMatch) {
      const path = normalizePath(contextPath(doc, line.from, lineIndent(before)));
      const names = listValueNamesFor(path, known, doc);
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
    const path = normalizePath(contextPath(doc, line.from, lineIndent(before.slice(0, word.from - line.from))));

    // Adding a new entry directly under the top-level `networks:` map: the
    // key itself is the network name, and (unlike `configs`/`secrets`) it
    // must match a network the cluster actually has, so it's worth
    // suggesting by name rather than leaving this map's keys unlisted.
    if (path.length === 1 && path[0] === 'networks' && known.networks.length > 0) {
      return {
        from: word.from,
        options: known.networks.map((n) => ({ label: n, type: 'text', apply: `${n}:` })),
        validFor: /^\S*$/,
      };
    }

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
