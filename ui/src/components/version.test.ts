import { describe, expect, it } from 'vitest';
import { planUpdatePath } from './version';

describe('planUpdatePath', () => {
  const tags = ['v0.19.0', 'v0.18.2', 'v0.18.0', 'v0.17.3', 'v0.17.0', 'v0.18.1'];
  it('takes the last patch of each series, oldest first', () => {
    expect(planUpdatePath('0.17.0', tags)).toEqual(['v0.17.3', 'v0.18.2', 'v0.19.0']);
  });
  it('skips what is not newer than the running version', () => {
    expect(planUpdatePath('0.17.3', tags)).toEqual(['v0.18.2', 'v0.19.0']);
    expect(planUpdatePath('0.19.0', tags)).toEqual([]);
    expect(planUpdatePath('v0.18.2', tags)).toEqual(['v0.19.0']);
  });
  it('crosses majors and ignores unparsable tags', () => {
    expect(planUpdatePath('0.9.1', ['nightly', 'v1.0.0', 'v0.9.4', 'v1.0.1-rc1', 'v1.0.2'])).toEqual(['v0.9.4', 'v1.0.2']);
    expect(planUpdatePath('weird', tags)).toEqual([]);
  });
});
