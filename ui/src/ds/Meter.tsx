import { cx } from './cx';

export type MeterTone = 'brand' | 'ok' | 'warn' | 'danger';

export function Meter({
  label,
  value,
  max = 100,
  unit,
  display,
  tone,
  className,
}: {
  label: string;
  value: number;
  max?: number;
  /** Suffix for the printed value; defaults to "%". */
  unit?: string;
  /** Replaces the printed value entirely, e.g. "3.1 / 8 GB". */
  display?: string;
  /** Defaults to the usual thresholds: warn from 75%, danger from 90%. */
  tone?: MeterTone;
  className?: string;
}) {
  const pct = Math.max(0, Math.min(100, (value / max) * 100));
  const t: MeterTone = tone ?? (pct >= 90 ? 'danger' : pct >= 75 ? 'warn' : 'brand');
  const shown = display ?? `${Math.round(value)}${unit ?? '%'}`;
  return (
    <div className={cx('cl-meter', className)}>
      <div className="cl-meter__head">
        <span className="cl-label">{label}</span>
        <span className="cl-meter__val">{shown}</span>
      </div>
      <div
        className="cl-meter__track"
        role="meter"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={max}
        aria-valuenow={value}
        aria-valuetext={shown + (t === 'danger' ? ', critical' : t === 'warn' ? ', high' : '')}
      >
        <div className={`cl-meter__fill cl-meter__fill--${t}`} style={{ width: `${pct}%` }} />
      </div>
    </div>
  );
}
