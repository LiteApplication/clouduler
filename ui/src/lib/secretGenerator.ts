// Validation and preview logic for the "generate a random value" secret
// creation path. Kept separate from the component so it's cheap to test.

import type { SecretCharset } from '../api/types';

export const CHARSET_SAMPLES: Record<SecretCharset, string> = {
  alphanumeric: 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789',
  hex: '0123456789abcdef',
  base64: 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/',
  printable: 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*()-_=+',
};

export const MIN_LENGTH = 1;
export const MAX_LENGTH = 1024;

export interface GeneratorValidation {
  valid: boolean;
  error: string | null;
}

export function validateGeneratorLength(length: number): GeneratorValidation {
  if (!Number.isInteger(length)) return { valid: false, error: 'length must be a whole number' };
  if (length < MIN_LENGTH) return { valid: false, error: `length must be at least ${MIN_LENGTH}` };
  if (length > MAX_LENGTH) return { valid: false, error: `length must be at most ${MAX_LENGTH}` };
  return { valid: true, error: null };
}

/** Deterministic sample of what the charset looks like, for the preview line. Not the actual secret. */
export function previewFor(charset: SecretCharset, length: number): string {
  const sample = CHARSET_SAMPLES[charset];
  const clipped = Math.min(length, 40);
  let out = '';
  for (let i = 0; i < clipped; i++) {
    out += sample[i % sample.length];
  }
  return out + (length > 40 ? '...' : '');
}
