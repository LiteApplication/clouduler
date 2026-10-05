import { describe, expect, it } from 'vitest';
import { durationToMs, fenceMarginWarning, isValidCidr, isValidDuration, isValidIp, isValidPort } from './validate';

describe('isValidIp', () => {
  it('accepts a plain ipv4 address', () => expect(isValidIp('10.10.0.1')).toBe(true));
  it('rejects an ipv4 octet over 255', () => expect(isValidIp('10.10.0.300')).toBe(false));
  it('accepts an ipv6 address', () => expect(isValidIp('2001:db8::5')).toBe(true));
  it('rejects garbage', () => expect(isValidIp('not-an-ip')).toBe(false));
});

describe('isValidCidr', () => {
  it('accepts a valid ipv4 subnet', () => expect(isValidCidr('10.10.0.0/24')).toBe(true));
  it('rejects a prefix out of range', () => expect(isValidCidr('10.10.0.0/40')).toBe(false));
  it('rejects a missing prefix', () => expect(isValidCidr('10.10.0.0')).toBe(false));
});

describe('isValidDuration', () => {
  it('accepts humantime style durations', () => {
    expect(isValidDuration('30s')).toBe(true);
    expect(isValidDuration('5m')).toBe(true);
    expect(isValidDuration('1h30m')).toBe(true);
  });
  it('rejects a bare number', () => expect(isValidDuration('30')).toBe(false));
  it('rejects nonsense', () => expect(isValidDuration('soon')).toBe(false));
});

describe('isValidPort', () => {
  it('accepts a normal port', () => expect(isValidPort(8443)).toBe(true));
  it('rejects 0', () => expect(isValidPort(0)).toBe(false));
  it('rejects above 65535', () => expect(isValidPort(70000)).toBe(false));
  it('rejects a non-integer', () => expect(isValidPort(80.5)).toBe(false));
});

describe('durationToMs', () => {
  it('sums compound durations', () => expect(durationToMs('1h30m')).toBe(5_400_000));
  it('returns null for nonsense', () => expect(durationToMs('soon')).toBeNull());
});

describe('fenceMarginWarning', () => {
  const f = (fence_timeout: string, node_timeout: string, stop_grace: string) => ({ fence_timeout, node_timeout, stop_grace });
  it('warns when fence plus grace reaches the node timeout', () => {
    expect(fenceMarginWarning(f('15s', '30s', '30s'))).toMatch(/two writers/);
    expect(fenceMarginWarning(f('15s', '45s', '30s'))).not.toBeNull();
  });
  it('is quiet when there is margin', () => expect(fenceMarginWarning(f('15s', '60s', '30s'))).toBeNull());
  it('is quiet while a field does not parse', () => expect(fenceMarginWarning(f('15s', '30s', '3'))).toBeNull());
});
