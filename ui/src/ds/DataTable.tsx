import type { ReactNode } from 'react';
import { cx } from './cx';

export interface Column<R> {
  key: string;
  label: string;
  mono?: boolean;
  align?: 'left' | 'right';
  render?: (value: unknown, row: R) => ReactNode;
}

/** `rows` need an `id` (or fall back to their index) for stable keys. */
export function DataTable<R extends { id?: string | number }>({
  columns,
  rows,
  caption,
  onRowClick,
  className,
}: {
  columns: Column<R>[];
  rows: R[];
  caption?: string;
  onRowClick?: (row: R) => void;
  className?: string;
}) {
  return (
    <div className={cx('cl-tablewrap', className)}>
      <table className="cl-table">
        {caption && <caption className="cl-sr">{caption}</caption>}
        <thead>
          <tr>
            {columns.map((c) => (
              <th key={c.key} scope="col" className={cx('cl-label', c.align === 'right' && 'cl-right')}>
                {c.label}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((r, i) => (
            <tr
              key={r.id ?? i}
              className={onRowClick ? 'cl-tr--press' : undefined}
              tabIndex={onRowClick ? 0 : undefined}
              onClick={onRowClick ? () => onRowClick(r) : undefined}
              onKeyDown={
                onRowClick
                  ? (e) => {
                      if (e.key === 'Enter' || e.key === ' ') {
                        e.preventDefault();
                        onRowClick(r);
                      }
                    }
                  : undefined
              }
            >
              {columns.map((c) => {
                const v = (r as Record<string, unknown>)[c.key];
                return (
                  <td
                    key={c.key}
                    data-label={c.label}
                    className={cx(c.mono && 'cl-td--mono', c.align === 'right' && 'cl-right')}
                  >
                    {c.render ? c.render(v, r) : (v as ReactNode)}
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
