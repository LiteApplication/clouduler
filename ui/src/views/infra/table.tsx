// A row-first wrapper over ds/DataTable for the infra views: columns render
// from the row, and an empty list shows a line of text instead of a bare head.

import type { ReactNode } from 'react';
import { DataTable as DsTable } from '../../ds';

export interface Col<R> {
  key: string;
  header: string;
  mono?: boolean;
  align?: 'left' | 'right';
  render?: (row: R) => ReactNode;
}

export function DataTable<R extends { id?: string | number }>({
  columns,
  rows,
  caption,
  empty,
  onRowClick,
}: {
  columns: Col<R>[];
  rows: R[];
  caption?: string;
  empty?: ReactNode;
  onRowClick?: (row: R) => void;
}) {
  if (rows.length === 0 && empty) return <p className="cl-sm cl-muted" style={{ padding: 'var(--space-4)' }}>{empty}</p>;
  return (
    <DsTable
      caption={caption}
      rows={rows}
      onRowClick={onRowClick}
      columns={columns.map((c) => ({
        key: c.key,
        label: c.header,
        mono: c.mono,
        align: c.align,
        render: c.render ? (_v: unknown, row: R) => c.render!(row) : undefined,
      }))}
    />
  );
}
