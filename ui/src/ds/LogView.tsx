import { cx } from './cx';

export type LogLevel = 'info' | 'warn' | 'error' | 'debug';

export interface LogViewLine {
  /** Already formatted, e.g. "12:04:31". */
  t: string;
  level?: LogLevel;
  msg: React.ReactNode;
}

const LV: Record<LogLevel, string> = { info: 'INFO', warn: 'WARN', error: 'ERR', debug: 'DBG' };

export function LogView({
  lines,
  wrap,
  label = 'Container logs',
  className,
}: {
  lines: LogViewLine[];
  /** Wrap long lines (phones) instead of scrolling sideways (desktop). */
  wrap?: boolean;
  label?: string;
  className?: string;
}) {
  return (
    <div className={cx('cl-log', wrap && 'cl-log--wrap', className)} role="log" aria-label={label} tabIndex={0}>
      {lines.map((l, i) => (
        <div key={i} className="cl-log__line">
          <span className="cl-log__t">{l.t}</span>
          <span className={`cl-log__lv cl-log__lv--${l.level ?? 'info'}`}>{LV[l.level ?? 'info']}</span>
          <span className="cl-log__msg">{l.msg}</span>
        </div>
      ))}
    </div>
  );
}
