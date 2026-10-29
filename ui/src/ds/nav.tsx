import type { KeyboardEvent, ReactNode } from 'react';
import { Icon, type IconName } from './Icon';
import { cx } from './cx';

export interface BottomNavItem {
  id: string;
  label: string;
  icon: IconName;
  badge?: number | string | null;
}

export function BottomNav({
  items,
  active,
  onChange,
  label = 'Main',
  className,
}: {
  items: BottomNavItem[];
  active?: string;
  onChange?: (id: string) => void;
  label?: string;
  className?: string;
}) {
  return (
    <nav className={cx('cl-bnav', className)} aria-label={label}>
      {items.map((it) => {
        const on = it.id === active;
        return (
          <button
            key={it.id}
            type="button"
            className={cx('cl-bnav__item', on && 'cl-bnav__item--on')}
            aria-current={on ? 'page' : undefined}
            onClick={() => onChange?.(it.id)}
          >
            <Icon name={it.icon} size={22} />
            <span>{it.label}</span>
            {it.badge != null && it.badge !== 0 && <span className="cl-bnav__badge cl-code-sm">{it.badge}</span>}
          </button>
        );
      })}
    </nav>
  );
}

export type SideRailEntry =
  | { heading: string }
  | {
      id: string;
      label: string;
      icon: IconName;
      badge?: number | string | null;
      indent?: boolean;
      /** Set on an item that opens and closes a group. */
      expanded?: boolean;
    };

export function SideRail({
  items,
  active,
  onChange,
  header,
  footer,
  label = 'Main',
  className,
}: {
  items: SideRailEntry[];
  active?: string;
  onChange?: (id: string) => void;
  header?: ReactNode;
  footer?: ReactNode;
  label?: string;
  className?: string;
}) {
  return (
    <nav className={cx('cl-rail', className)} aria-label={label}>
      {header && <div className="cl-rail__head">{header}</div>}
      <ul className="cl-rail__list">
        {items.map((it) => {
          if ('heading' in it) {
            return (
              <li key={it.heading} className="cl-label cl-rail__group">
                {it.heading}
              </li>
            );
          }
          const on = it.id === active;
          return (
            <li key={it.id}>
              <button
                type="button"
                className={cx('cl-rail__item', on && 'cl-rail__item--on', it.indent && 'cl-rail__item--indent')}
                aria-current={on ? 'page' : undefined}
                aria-expanded={it.expanded}
                onClick={() => onChange?.(it.id)}
              >
                <Icon name={it.icon} size={20} />
                <span>{it.label}</span>
                {it.badge != null && it.badge !== 0 && <span className="cl-rail__badge cl-code-sm">{it.badge}</span>}
              </button>
            </li>
          );
        })}
      </ul>
      {footer && <div className="cl-rail__foot">{footer}</div>}
    </nav>
  );
}

export interface TabItem {
  id: string;
  label: string;
  count?: number | string | null;
}

export function Tabs({
  items,
  active,
  onChange,
  label,
  className,
}: {
  items: TabItem[];
  active: string;
  onChange?: (id: string) => void;
  label?: string;
  className?: string;
}) {
  const onKey = (e: KeyboardEvent<HTMLDivElement>) => {
    const i = items.findIndex((x) => x.id === active);
    let n: number;
    if (e.key === 'ArrowRight') n = (i + 1) % items.length;
    else if (e.key === 'ArrowLeft') n = (i - 1 + items.length) % items.length;
    else return;
    e.preventDefault();
    onChange?.(items[n].id);
    (e.currentTarget.querySelectorAll('[role=tab]')[n] as HTMLElement | undefined)?.focus();
  };
  return (
    <div className={cx('cl-tabs', className)} role="tablist" aria-label={label} onKeyDown={onKey}>
      {items.map((it) => {
        const on = it.id === active;
        return (
          <button
            key={it.id}
            type="button"
            role="tab"
            aria-selected={on}
            tabIndex={on ? 0 : -1}
            className={cx('cl-tab', on && 'cl-tab--on')}
            onClick={() => onChange?.(it.id)}
          >
            <span>{it.label}</span>
            {it.count != null && it.count !== 0 && <span className="cl-tab__count">{it.count}</span>}
          </button>
        );
      })}
    </div>
  );
}

export function Chip({
  label,
  selected,
  icon,
  count,
  disabled,
  onClick,
  className,
}: {
  label: string;
  selected?: boolean;
  icon?: IconName;
  count?: number | string | null;
  disabled?: boolean;
  onClick?: () => void;
  className?: string;
}) {
  return (
    <button
      type="button"
      className={cx('cl-chip', selected && 'cl-chip--on', className)}
      aria-pressed={!!selected}
      disabled={disabled}
      onClick={onClick}
    >
      {icon && <Icon name={icon} size={16} />}
      <span>{label}</span>
      {count != null && <span className="cl-chip__count">{count}</span>}
    </button>
  );
}
