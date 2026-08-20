import type { ReactNode } from 'react';
import type { ChartTone } from './Donut';

const TONE_VAR: Record<ChartTone, string> = {
  good: 'var(--good)',
  warn: 'var(--warn)',
  bad: 'var(--bad)',
  neutral: 'var(--text-faint)',
  primary: 'var(--primary)',
  secondary: 'var(--secondary)',
  accent: 'var(--accent)',
};

/** Horizontal fill gauge: label + value on top, a track underneath. */
export function Meter({
  label,
  value,
  max,
  display,
  tone = 'primary',
}: {
  label: ReactNode;
  value: number;
  max: number;
  /** Text shown at the right of the label row; defaults to "value/max". */
  display?: string;
  tone?: ChartTone;
}) {
  const pct = max > 0 ? Math.min(100, Math.max(0, (value / max) * 100)) : 0;
  return (
    <div className="meter">
      <div className="meter-head">
        <span>{label}</span>
        <span className="mono faint">{display ?? `${value}/${max}`}</span>
      </div>
      <div className="meter-track">
        <div className="meter-fill" style={{ width: `${pct}%`, background: TONE_VAR[tone] }} />
      </div>
    </div>
  );
}
