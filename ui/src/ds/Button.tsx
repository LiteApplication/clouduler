import type { ReactNode } from 'react';
import { Icon, type IconName } from './Icon';
import { cx } from './cx';

export interface ButtonProps {
  variant?: 'primary' | 'secondary' | 'ghost' | 'danger';
  size?: 'md' | 'sm';
  icon?: IconName;
  /** Icon-only button; `label` (or string children) becomes its aria-label. */
  iconOnly?: boolean;
  label?: string;
  loading?: boolean;
  disabled?: boolean;
  type?: 'button' | 'submit';
  className?: string;
  title?: string;
  onClick?: () => void;
  children?: ReactNode;
}

export function Button({
  variant = 'secondary',
  size = 'md',
  icon,
  iconOnly,
  label,
  loading,
  disabled,
  type = 'button',
  className,
  title,
  onClick,
  children,
}: ButtonProps) {
  return (
    <button
      type={type}
      className={cx('cl-btn', `cl-btn--${variant}`, `cl-btn--${size}`, iconOnly && 'cl-btn--icon', className)}
      disabled={disabled || loading}
      onClick={onClick}
      title={title}
      aria-label={iconOnly ? (label ?? (typeof children === 'string' ? children : undefined)) : undefined}
      aria-busy={loading || undefined}
    >
      {loading ? <Icon name="sync" className="cl-spin" /> : icon ? <Icon name={icon} /> : null}
      {iconOnly ? null : <span>{children}</span>}
    </button>
  );
}
