// Small SVG donut chart with a side legend. No charting library: this is a
// handful of stroke-dasharray arcs, which is all a cluster-sized dataset
// (a few nodes, a few states) ever needs.

import type { BadgeTone } from './Badge';

export type ChartTone = BadgeTone | 'primary' | 'secondary' | 'accent';

export interface DonutSegment {
  label: string;
  value: number;
  tone: ChartTone;
}

const TONE_VAR: Record<ChartTone, string> = {
  good: 'var(--good)',
  warn: 'var(--warn)',
  bad: 'var(--bad)',
  neutral: 'var(--text-faint)',
  primary: 'var(--primary)',
  secondary: 'var(--secondary)',
  accent: 'var(--accent)',
};

export function Donut({
  segments,
  size = 128,
  thickness = 16,
  centerLabel,
}: {
  segments: DonutSegment[];
  size?: number;
  thickness?: number;
  /** Shown in the hole; defaults to the total. */
  centerLabel?: string;
}) {
  const total = segments.reduce((sum, s) => sum + s.value, 0);
  const radius = (size - thickness) / 2;
  const circumference = 2 * Math.PI * radius;
  let offset = 0;

  return (
    <div className="donut-row">
      <svg width={size} height={size} viewBox={`0 0 ${size} ${size}`} className="donut" role="img" aria-label="breakdown chart">
        <circle
          cx={size / 2}
          cy={size / 2}
          r={radius}
          fill="none"
          stroke="var(--surface-2)"
          strokeWidth={thickness}
        />
        {total > 0 &&
          segments
            .filter((s) => s.value > 0)
            .map((s, i) => {
              const fraction = s.value / total;
              const length = fraction * circumference;
              const dash = `${length} ${circumference - length}`;
              const el = (
                <circle
                  key={i}
                  cx={size / 2}
                  cy={size / 2}
                  r={radius}
                  fill="none"
                  stroke={TONE_VAR[s.tone]}
                  strokeWidth={thickness}
                  strokeDasharray={dash}
                  strokeDashoffset={-offset}
                  transform={`rotate(-90 ${size / 2} ${size / 2})`}
                  strokeLinecap={segments.filter((x) => x.value > 0).length > 1 ? 'butt' : 'round'}
                >
                  <title>{`${s.label}: ${s.value}`}</title>
                </circle>
              );
              offset += length;
              return el;
            })}
        <text
          x="50%"
          y="50%"
          textAnchor="middle"
          dominantBaseline="central"
          className="donut-center"
        >
          {centerLabel ?? total}
        </text>
      </svg>
      <ul className="donut-legend">
        {segments.map((s) => (
          <li key={s.label}>
            <span className="donut-dot" style={{ background: TONE_VAR[s.tone] }} />
            <span className="donut-legend-label">{s.label}</span>
            <span className="donut-legend-value">{s.value}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}
