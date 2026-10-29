// The ds Icon is the app's one icon set (components/Icon.tsx) with the bundle's
// `size`/`label` props, so a ds component and an old view draw the same glyphs.

import { Icon as BaseIcon, type IconName } from '../components/Icon';
import { cx } from './cx';

export type { IconName };

export interface IconProps {
  name: IconName;
  size?: number;
  className?: string;
  /** Accessible name. Without one the icon is decorative and hidden from AT. */
  label?: string;
}

export function Icon({ name, size = 20, className, label }: IconProps) {
  return <BaseIcon name={name} size={size} className={cx('cl-icon', className)} title={label} />;
}
