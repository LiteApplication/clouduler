// Structural diff between two cluster configs, shown to the operator before
// they apply an edit. Walks both trees in lockstep so the diff reads in the
// same order the form does, rather than alphabetically.

export interface DiffEntry {
  path: string;
  before: string;
  after: string;
}

const MISSING = Symbol('missing');

function stringify(value: unknown): string {
  if (value === undefined || value === null) return '(none)';
  if (typeof value === 'string') return value;
  if (typeof value === 'number' || typeof value === 'boolean') return String(value);
  return JSON.stringify(value);
}

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/** Deep diff producing a leaf-level entry for every changed, added or removed field. */
export function diffConfig(before: unknown, after: unknown, path = ''): DiffEntry[] {
  if (before === after) return [];

  const bothObjects = isPlainObject(before) && isPlainObject(after);
  const bothArrays = Array.isArray(before) && Array.isArray(after);

  if (bothObjects) {
    const keys = new Set([...Object.keys(before), ...Object.keys(after)]);
    const entries: DiffEntry[] = [];
    for (const key of [...keys].sort()) {
      const childPath = path ? `${path}.${key}` : key;
      const b = key in before ? before[key] : MISSING;
      const a = key in after ? after[key] : MISSING;
      if (b === MISSING) {
        entries.push({ path: childPath, before: '(none)', after: stringify(a) });
      } else if (a === MISSING) {
        entries.push({ path: childPath, before: stringify(b), after: '(none)' });
      } else {
        entries.push(...diffConfig(b, a, childPath));
      }
    }
    return entries;
  }

  if (bothArrays) {
    const b = before as unknown[];
    const a = after as unknown[];
    if (JSON.stringify(b) === JSON.stringify(a)) return [];
    return [{ path, before: stringify(b), after: stringify(a) }];
  }

  if (JSON.stringify(before) === JSON.stringify(after)) return [];
  return [{ path: path || '(root)', before: stringify(before), after: stringify(after) }];
}
