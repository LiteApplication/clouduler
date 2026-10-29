import type { ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { cx } from './cx';

export interface Crumb {
  label: string;
  to?: string;
}

export function PageHeader({
  eyebrow,
  title,
  sub,
  actions,
  breadcrumb,
}: {
  eyebrow?: string;
  title: ReactNode;
  sub?: ReactNode;
  actions?: ReactNode;
  breadcrumb?: Crumb[];
}) {
  return (
    <header className="hd">
      <div className="hd__t">
        {breadcrumb && breadcrumb.length > 0 && (
          <nav aria-label="Breadcrumb" className="crumbs">
            {breadcrumb.map((c, i) => (
              <span key={`${c.label}-${i}`}>
                {c.to ? <Link to={c.to}>{c.label}</Link> : <span aria-current="page">{c.label}</span>}
                {i < breadcrumb.length - 1 && <span aria-hidden="true"> / </span>}
              </span>
            ))}
          </nav>
        )}
        {eyebrow && <span className="eyebrow">{eyebrow}</span>}
        <h1 className="h1">{title}</h1>
        {sub && <p className="sub">{sub}</p>}
      </div>
      {actions && <div className="acts">{actions}</div>}
    </header>
  );
}

export function Panel({
  title,
  actions,
  flush,
  children,
  className,
  as: Tag = 'section',
}: {
  title?: ReactNode;
  actions?: ReactNode;
  /** No padding, for tables and lists that bring their own. */
  flush?: boolean;
  children?: ReactNode;
  className?: string;
  as?: 'section' | 'div';
}) {
  return (
    <Tag className={cx('panel', flush && 'panel--flush', className)}>
      {(title || actions) && (
        <div className="panel__h">
          {title && <h2 className="h2">{title}</h2>}
          {actions && <div className="acts">{actions}</div>}
        </div>
      )}
      {children}
    </Tag>
  );
}

/** Label/value pairs. Values are monospace, labels uppercase. */
export function KV({ items, className }: { items: [ReactNode, ReactNode][]; className?: string }) {
  return (
    <dl className={cx('kv', className)}>
      {items.map(([k, v], i) => (
        <div key={i} style={{ display: 'contents' }}>
          <dt>{k}</dt>
          <dd>{v}</dd>
        </div>
      ))}
    </dl>
  );
}

export type TagTone = 'brand' | 'pop' | 'warn' | 'ok' | 'danger';

export function Tag({ tone, children, className }: { tone?: TagTone; children: ReactNode; className?: string }) {
  return <span className={cx('tag', tone && `tag--${tone}`, className)}>{children}</span>;
}

/** SVG sparkline of a number series, scaled to its own min and max (or to
 *  `max` when the series is a percentage). */
export function Spark({
  values,
  max,
  tone = 'brand',
  label,
  className,
}: {
  values: number[];
  max?: number;
  tone?: 'brand' | 'ok' | 'warn' | 'danger' | 'pop';
  /** Accessible summary; without it the chart is decorative. */
  label?: string;
  className?: string;
}) {
  const w = 120;
  const h = 44;
  const pad = 3;
  let d = '';
  if (values.length > 0) {
    const hi = max ?? Math.max(...values, 1);
    const lo = max != null ? 0 : Math.min(...values, 0);
    const span = hi - lo || 1;
    const step = values.length > 1 ? (w - pad * 2) / (values.length - 1) : 0;
    d = values
      .map((v, i) => {
        const x = values.length > 1 ? pad + i * step : w / 2;
        const y = h - pad - ((Math.min(Math.max(v, lo), hi) - lo) / span) * (h - pad * 2);
        return `${i === 0 ? 'M' : 'L'}${x.toFixed(1)} ${y.toFixed(1)}`;
      })
      .join(' ');
  }
  return (
    <svg
      className={cx('spark', className)}
      viewBox={`0 0 ${w} ${h}`}
      preserveAspectRatio="none"
      role={label ? 'img' : undefined}
      aria-label={label}
      aria-hidden={label ? undefined : true}
    >
      <path d={d} style={{ stroke: `var(--${tone})` }} vectorEffect="non-scaling-stroke" />
    </svg>
  );
}

/** Step progress for a migration or update: `step` of `total`, 1-based. */
export function StepBar({ step, total, label }: { step: number; total: number; label?: string }) {
  return (
    <span className="steps stepbar" role="img" aria-label={label ?? `Step ${step} of ${total}`}>
      {Array.from({ length: total }, (_, i) => (
        <i key={i} className={i < step ? 'on' : undefined} />
      ))}
    </span>
  );
}
