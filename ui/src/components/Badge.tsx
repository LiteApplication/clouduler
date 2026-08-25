import type { ReactNode } from 'react';

export type BadgeTone = 'good' | 'warn' | 'bad' | 'neutral';

export function Badge({
  tone,
  children,
  title,
}: {
  tone: BadgeTone;
  children: ReactNode;
  /** Shown as a native tooltip, e.g. the reason behind a "pending_placement" state. */
  title?: string;
}) {
  return (
    <span className={`badge badge-${tone}`} title={title}>
      {children}
    </span>
  );
}
