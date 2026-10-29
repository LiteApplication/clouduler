import { describe, expect, it } from 'vitest';
import { diffLines, lineForPath } from './specText';

const spec = `services:
  web:
    image: nginx
  db:
    image: postgres
`;

describe('lineForPath', () => {
  it('finds the line a dotted path names', () => {
    expect(lineForPath(spec, 'services.db.image')).toBe(5);
    expect(lineForPath(spec, 'services.web')).toBe(2);
  });

  it('falls back to the deepest key that matched', () => {
    expect(lineForPath(spec, 'services.web.ports')).toBe(2);
  });

  it('returns null when nothing matches', () => {
    expect(lineForPath(spec, 'volumes')).toBeNull();
    expect(lineForPath(spec, '')).toBeNull();
  });
});

describe('diffLines', () => {
  it('marks added and removed lines and trims context', () => {
    const next = spec.replace('nginx', 'caddy');
    const d = diffLines(spec, next, 1);
    expect(d.filter((l) => l.kind === 'del').map((l) => l.text)).toEqual(['    image: nginx']);
    expect(d.filter((l) => l.kind === 'add').map((l) => l.text)).toEqual(['    image: caddy']);
  });

  it('is empty when nothing changed', () => {
    expect(diffLines(spec, spec)).toEqual([]);
  });
});
