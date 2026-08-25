// Display formatting helpers shared across views.

import type { InstanceState } from '../api/types';
import type { BadgeTone } from '../components/Badge';

/** Badge tone for an instance's `state`: "running" reads good, a crash or a
 *  service with nowhere to go reads bad, and everything mid-transition
 *  (pulling, restarting, stopping) reads warn rather than bad — it isn't
 *  broken, just not settled yet. */
export function instanceStateTone(state: InstanceState['state']): BadgeTone {
  switch (state) {
    case 'running':
      return 'good';
    case 'pulling':
    case 'restarting':
    case 'stopping':
      return 'warn';
    case 'exited':
    case 'pending_placement':
      return 'bad';
    case 'stopped':
    case 'unknown':
    default:
      return 'neutral';
  }
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return '-';
  if (bytes === 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
  const exp = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** exp;
  return `${value.toFixed(exp === 0 ? 0 : 1)} ${units[exp]}`;
}

export function formatUptime(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return '-';
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const mins = Math.floor((seconds % 3600) / 60);
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${mins}m`;
  return `${mins}m`;
}

export function formatPercent(fraction: number): string {
  return `${(fraction * 100).toFixed(1)}%`;
}

export function formatTimestamp(iso: string | null | undefined): string {
  if (!iso) return '-';
  try {
    return new Date(iso).toLocaleString();
  } catch {
    return iso;
  }
}

export function formatUnixSeconds(secs: number): string {
  return new Date(secs * 1000).toLocaleString();
}

export function daysUntil(unixSeconds: number): number {
  return Math.ceil((unixSeconds * 1000 - Date.now()) / 86_400_000);
}
