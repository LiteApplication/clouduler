// Turns the log stream's lines into ds LogView lines: a clock time instead of
// an ISO timestamp, ansi colour codes rendered, and a level guessed from the
// text when the line is not on stderr.

import type { LogLevel, LogViewLine } from '../../ds';
import { LogMessage } from '../../lib/ansi';

export interface StreamLine {
  timestamp: string;
  message: string;
  stream: 'stdout' | 'stderr';
}

const LEVELS: [RegExp, LogLevel][] = [
  [/\b(fatal|panic|error|err|crit(ical)?)\b/i, 'error'],
  [/\b(warn(ing)?)\b/i, 'warn'],
  [/\b(debug|trace|dbg)\b/i, 'debug'],
];

export function levelOf(line: StreamLine): LogLevel {
  for (const [re, level] of LEVELS) if (re.test(line.message)) return level;
  return line.stream === 'stderr' ? 'warn' : 'info';
}

export function clockTime(timestamp: string | number): string {
  const d = new Date(timestamp);
  if (Number.isNaN(d.getTime())) return '--:--:--';
  return d.toLocaleTimeString([], { hour12: false });
}

export function toLogViewLine(line: StreamLine): LogViewLine {
  return { t: clockTime(line.timestamp), level: levelOf(line), msg: <LogMessage text={line.message} /> };
}

/** The `useEventSource` options every container log view shares. */
export function logStreamOptions(limit: number) {
  return {
    limit,
    events: ['stdout', 'stderr'],
    mapItem: (data: unknown, eventType: string): StreamLine => {
      const line = data as { text: string; timestamp: string };
      return { timestamp: line.timestamp, message: line.text, stream: eventType as 'stdout' | 'stderr' };
    },
    dedupeKey: (line: StreamLine) => `${line.timestamp}|${line.stream}|${line.message}`,
  };
}
