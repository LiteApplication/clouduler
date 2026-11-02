// Structural comparison of a JSON value against a fixture captured from the
// real Rust server (src/api/__fixtures__/api-shapes.json). Used by the mock
// backend's contract test; written as a plain helper so any other test can
// reuse it.
//
// Rules, applied recursively:
//  * both directions: a key the fixture has must be in the value (unless the
//    fixture did not have it in every sample, i.e. the server omits it when
//    empty), and a key the value has must be in the fixture (unless the path
//    is whitelisted in `optional`, because the harness that produced the
//    fixture never had that data to show).
//  * JSON types must match. A null in the fixture means "type unknown", so
//    anything goes there; a null in the value is fine where the fixture has
//    null somewhere or the path is optional.
//  * arrays: every element is compared against the merge of all fixture
//    elements. An empty fixture array proves nothing and passes.
//  * `maps` names paths that are dictionaries keyed by data (node names,
//    instance ids): every value is compared against the merged fixture values
//    and the keys themselves are free.
//  * `any` names paths whose shape is a tagged union the fixture only shows
//    one arm of.
//
// Paths: `.key`, `[]` for array items, `.*` for map values.

export interface ShapeOptions {
  maps?: string[];
  optional?: string[];
  any?: string[];
}

type Kind = 'null' | 'array' | 'object' | 'string' | 'number' | 'boolean';

interface Schema {
  kinds: Set<Kind>;
  nullable: boolean;
  objects: number;
  props: Map<string, { schema: Schema; count: number }>;
  item: Schema | null;
}

export function kindOf(v: unknown): Kind {
  if (v === null || v === undefined) return 'null';
  if (Array.isArray(v)) return 'array';
  switch (typeof v) {
    case 'object':
      return 'object';
    case 'string':
      return 'string';
    case 'number':
      return 'number';
    case 'boolean':
      return 'boolean';
    default:
      return 'null';
  }
}

function infer(samples: unknown[], path: string, opts: ShapeOptions): Schema {
  const schema: Schema = { kinds: new Set(), nullable: false, objects: 0, props: new Map(), item: null };
  const arrays: unknown[] = [];
  const mapValues: unknown[] = [];
  const keyed = new Map<string, unknown[]>();
  for (const s of samples) {
    const k = kindOf(s);
    if (k === 'null') {
      schema.nullable = true;
      continue;
    }
    schema.kinds.add(k);
    if (k === 'array') arrays.push(...(s as unknown[]));
    if (k === 'object') {
      schema.objects += 1;
      for (const [key, val] of Object.entries(s as Record<string, unknown>)) {
        if (opts.maps?.includes(path)) mapValues.push(val);
        else keyed.set(key, [...(keyed.get(key) ?? []), val]);
      }
    }
  }
  if (arrays.length > 0) schema.item = infer(arrays, `${path}[]`, opts);
  if (mapValues.length > 0) schema.item = infer(mapValues, `${path}.*`, opts);
  for (const [key, vals] of keyed) {
    schema.props.set(key, { schema: infer(vals, `${path}.${key}`, opts), count: vals.length });
  }
  return schema;
}

function check(value: unknown, schema: Schema, path: string, opts: ShapeOptions, out: string[]) {
  if (opts.any?.includes(path)) return;
  const kind = kindOf(value);
  const unknownShape = schema.kinds.size === 0;
  if (kind === 'null') {
    if (!schema.nullable && !unknownShape && !opts.optional?.includes(path)) {
      out.push(`${path || '<root>'}: null, but the server never sends null here`);
    }
    return;
  }
  if (unknownShape) return; // the fixture only ever saw null: nothing to compare to
  if (!schema.kinds.has(kind)) {
    out.push(`${path || '<root>'}: is ${kind}, the server sends ${[...schema.kinds].join('|')}`);
    return;
  }
  if (kind === 'array') {
    if (schema.item) (value as unknown[]).forEach((v) => check(v, schema.item!, `${path}[]`, opts, out));
    return;
  }
  if (kind !== 'object') return;
  const obj = value as Record<string, unknown>;
  if (opts.maps?.includes(path)) {
    if (schema.item) for (const v of Object.values(obj)) check(v, schema.item, `${path}.*`, opts, out);
    return;
  }
  for (const [key, v] of Object.entries(obj)) {
    const p = schema.props.get(key);
    if (!p) {
      if (!opts.optional?.includes(`${path}.${key}`)) out.push(`${path}.${key}: the server never sends this key`);
      continue;
    }
    check(v, p.schema, `${path}.${key}`, opts, out);
  }
  for (const [key, p] of schema.props) {
    const required = p.count === schema.objects;
    if (required && !(key in obj) && !opts.optional?.includes(`${path}.${key}`)) {
      out.push(`${path}.${key}: missing (the server always sends it)`);
    }
  }
}

/** Human-readable differences; empty means the value has the fixture's shape. */
export function shapeDiffs(value: unknown, fixture: unknown, opts: ShapeOptions = {}): string[] {
  const out: string[] = [];
  check(value, infer([fixture], '', opts), '', opts, out);
  return out;
}
