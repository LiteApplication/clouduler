import { describe, expect, it } from 'vitest';
import { parseDotenv } from './dotenv';

const vals = (t: string) => Object.fromEntries(parseDotenv(t).entries.map((e) => [e.key, e.value]));

describe('parseDotenv', () => {
  it('reads plain, exported, empty and commented lines', () => {
    const r = parseDotenv('# c\n\nA=1\nexport B=two words\nC=\n  D = x\n');
    expect(r.errors).toEqual([]);
    expect(vals('# c\n\nA=1\nexport B=two words\nC=\n  D = x\n')).toEqual({ A: '1', B: 'two words', C: '', D: 'x' });
  });
  it('strips inline comments only after whitespace', () => {
    expect(vals('A=x # note\nB=x#y\nC= # all comment')).toEqual({ A: 'x', B: 'x#y', C: '' });
  });
  it('handles quotes and escapes', () => {
    expect(vals('A="a\\nb # not"  # c\nB=\'a\\nb\'\nC="q\\"q"\nD="x\\qy"')).toEqual({ A: 'a\nb # not', B: 'a\\nb', C: 'q"q', D: 'x\\qy' });
  });
  it('supports multiline quoted values and CRLF', () => {
    const r = parseDotenv('K="l1\r\nl2"\r\nN=1\r\n');
    expect(r.errors).toEqual([]);
    expect(vals('K="l1\r\nl2"\r\nN=1\r\n')).toEqual({ K: 'l1\nl2', N: '1' });
  });
  it('reports bad lines with line numbers', () => {
    const r = parseDotenv('A=1\n=x\nBARE\nB="oops\n');
    expect(r.errors.map((e) => e.line)).toEqual([2, 3, 4]);
    expect(r.entries.map((e) => e.key)).toEqual(['A']);
  });
  it('rejects text after a closing quote', () => {
    const r = parseDotenv('A="x" y\nB=1');
    expect(r.errors.map((e) => e.line)).toEqual([1]);
    expect(vals('A="x" y\nB=1')).toEqual({ B: '1' });
  });
  it('last duplicate wins and warns', () => {
    const r = parseDotenv('A=1\nB=2\nA=3');
    expect(r.entries.map((e) => [e.key, e.value])).toEqual([['A', '3'], ['B', '2']]);
    expect(r.warnings).toHaveLength(1);
    expect(r.warnings[0].line).toBe(3);
  });
  it('keeps = inside values', () => {
    expect(vals('URL=postgres://u:p@h/db?a=b')).toEqual({ URL: 'postgres://u:p@h/db?a=b' });
  });
});
