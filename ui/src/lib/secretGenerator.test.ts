import { describe, expect, it } from 'vitest';
import { MAX_LENGTH, MIN_LENGTH, previewFor, validateGeneratorLength } from './secretGenerator';

describe('secret generator validation', () => {
  it('accepts a normal length', () => {
    expect(validateGeneratorLength(32)).toEqual({ valid: true, error: null });
  });

  it('rejects a non-integer length', () => {
    expect(validateGeneratorLength(12.5).valid).toBe(false);
  });

  it('rejects below the minimum', () => {
    expect(validateGeneratorLength(MIN_LENGTH - 1).valid).toBe(false);
  });

  it('rejects above the maximum', () => {
    expect(validateGeneratorLength(MAX_LENGTH + 1).valid).toBe(false);
  });

  it('accepts the boundary values', () => {
    expect(validateGeneratorLength(MIN_LENGTH).valid).toBe(true);
    expect(validateGeneratorLength(MAX_LENGTH).valid).toBe(true);
  });
});

describe('secret generator preview', () => {
  it('produces a preview of the requested length for short values', () => {
    expect(previewFor('hex', 8)).toHaveLength(8);
  });

  it('truncates and marks long previews instead of rendering the full length', () => {
    const preview = previewFor('printable', 200);
    expect(preview.endsWith('...')).toBe(true);
    expect(preview.length).toBeLessThan(200);
  });

  it('only uses characters from the selected charset', () => {
    const preview = previewFor('hex', 20);
    expect(preview).toMatch(/^[0-9a-f]+$/);
  });
});
