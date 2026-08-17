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
