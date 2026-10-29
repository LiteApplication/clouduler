import { describe, expect, it } from 'vitest';
import { relativeTime } from './format';

// A fixed "now" in milliseconds, so every expectation is exact.
const NOW = Date.UTC(2026, 9, 29, 12, 0, 0);
const nowSecs = NOW / 1000;

describe('relativeTime', () => {
  it('reads a number below 1e12 as unix seconds', () => {
    expect(relativeTime(nowSecs - 3 * 3600, NOW)).toBe('3 hours ago');
    expect(relativeTime(nowSecs + 3 * 86400, NOW)).toBe('in 3 days');
  });

  it('reads a number above 1e12 as unix milliseconds', () => {
    expect(relativeTime(NOW - 2 * 86400_000, NOW)).toBe('2 days ago');
    expect(relativeTime(NOW + 5 * 60_000, NOW)).toBe('in 5 minutes');
  });

  it('gives the same answer for seconds and for the same instant in milliseconds', () => {
    expect(relativeTime(nowSecs - 600, NOW)).toBe(relativeTime((nowSecs - 600) * 1000, NOW));
  });

  it('does not produce a years-scale answer for a milliseconds value that a caller multiplied by 1000 again', () => {
    // The Checked in 56808 years bug: a ms value was multiplied by 1000 once more.
    expect(relativeTime(NOW - 60_000, NOW)).toBe('1 minute ago');
  });

  it('says just now inside 45 seconds in either unit', () => {
    expect(relativeTime(nowSecs - 10, NOW)).toBe('just now');
    expect(relativeTime(NOW - 10_000, NOW)).toBe('just now');
  });

  it('parses ISO strings and Dates', () => {
    const iso = new Date(NOW - 3 * 86400_000).toISOString();
    expect(relativeTime(iso, NOW)).toBe('3 days ago');
    expect(relativeTime(new Date(NOW - 3 * 86400_000), NOW)).toBe('3 days ago');
  });

  it('returns a dash for null, undefined, empty and unparseable input', () => {
    expect(relativeTime(null, NOW)).toBe('-');
    expect(relativeTime(undefined, NOW)).toBe('-');
    expect(relativeTime('', NOW)).toBe('-');
    expect(relativeTime('not a date', NOW)).toBe('-');
  });
});
