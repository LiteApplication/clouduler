// Small, dependency-free validators for the config form. Rust's humantime
// and ipnet crates are the real authority; these just catch obvious mistakes
// before a submit round-trips to the server.

const IPV4 = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/;

export function isValidIpv4(value: string): boolean {
  const m = IPV4.exec(value);
  if (!m) return false;
  return m.slice(1, 5).every((octet) => Number(octet) <= 255);
}

export function isValidIpv6(value: string): boolean {
  if (!value.includes(':')) return false;
  // Not a full RFC 4291 parser, just enough to reject obvious garbage:
  // hex groups, at most one "::" collapse, and 2-8 groups either way.
  if ((value.match(/::/g) || []).length > 1) return false;
  const groups = value.split(':').filter((g) => g.length > 0);
  return groups.length >= 2 && groups.length <= 8 && groups.every((g) => /^[0-9a-fA-F]{1,4}$/.test(g));
}

export function isValidIp(value: string): boolean {
  return isValidIpv4(value) || isValidIpv6(value);
}

export function isValidCidr(value: string): boolean {
  const [addr, prefix] = value.split('/');
  if (!addr || prefix === undefined) return false;
  const bits = Number(prefix);
  if (!Number.isInteger(bits)) return false;
  if (isValidIpv4(addr)) return bits >= 0 && bits <= 32;
  if (isValidIpv6(addr)) return bits >= 0 && bits <= 128;
  return false;
}

// humantime accepts things like "30s", "5m", "1h30m", "2days". Keep this
// permissive; the server is the real validator.
const DURATION = /^(\d+\s*(ns|us|µs|ms|s|sec|secs|m|min|mins|h|hour|hours|d|day|days|w|week|weeks)\s*)+$/i;

export function isValidDuration(value: string): boolean {
  return DURATION.test(value.trim());
}

export function isValidHostname(value: string): boolean {
  if (value.length === 0 || value.length > 253) return false;
  const labels = value.split('.');
  return labels.every((label) => /^[a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?$/.test(label));
}

export function isValidPort(value: number): boolean {
  return Number.isInteger(value) && value > 0 && value <= 65535;
}

const DURATION_UNIT_MS: Record<string, number> = {
  ns: 1e-6, us: 1e-3, 'µs': 1e-3, ms: 1,
  s: 1000, sec: 1000, secs: 1000,
  m: 60_000, min: 60_000, mins: 60_000,
  h: 3_600_000, hour: 3_600_000, hours: 3_600_000,
  d: 86_400_000, day: 86_400_000, days: 86_400_000,
  w: 604_800_000, week: 604_800_000, weeks: 604_800_000,
};

/** Milliseconds in a humantime string, or null if it does not parse. */
export function durationToMs(value: string): number | null {
  if (!isValidDuration(value)) return null;
  let total = 0;
  for (const [, n, unit] of value.matchAll(/(\d+)\s*([a-zµ]+)/gi)) {
    total += Number(n) * DURATION_UNIT_MS[unit.toLowerCase()];
  }
  return total;
}

/**
 * Mirrors `FailoverConfig::fence_margin_warning` in cl-config: null when the
 * three durations are fine (or any of them does not parse yet).
 */
export function fenceMarginWarning(f: {
  fence_timeout: string;
  node_timeout: string;
  stop_grace: string;
}): string | null {
  const fence = durationToMs(f.fence_timeout);
  const node = durationToMs(f.node_timeout);
  const grace = durationToMs(f.stop_grace);
  if (fence === null || node === null || grace === null) return null;
  if (fence + grace < node) return null;
  return (
    `Fence timeout (${f.fence_timeout.trim()}) plus the stop grace (${f.stop_grace.trim()}) reaches the node timeout (${f.node_timeout.trim()}). ` +
    'A container that takes its full grace to stop may still be running when the leader reschedules it elsewhere, so a replicated volume could see two writers. ' +
    'Raise the node timeout, or lower the fence timeout or the stop grace.'
  );
}
