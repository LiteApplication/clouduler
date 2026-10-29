// Inline SVG icon set. These are drawn here rather than pulled from an icon
// package so the UI ships no extra dependency and works fully offline as a
// PWA: every glyph is part of the JS bundle and inherits currentColor.

import type { SVGProps } from 'react';

export type IconName =
  | 'overview'
  | 'nodes'
  | 'stacks'
  | 'instances'
  | 'secrets'
  | 'drives'
  | 'settings'
  | 'certificate'
  | 'menu'
  | 'close'
  | 'plus'
  | 'trash'
  | 'refresh'
  | 'play'
  | 'stop'
  | 'restart'
  | 'move'
  | 'edit'
  | 'upload'
  | 'download'
  | 'check'
  | 'check-circle'
  | 'alert'
  | 'alert-circle'
  | 'info'
  | 'search'
  | 'filter'
  | 'sun'
  | 'moon'
  | 'system'
  | 'key'
  | 'lock'
  | 'file'
  | 'terminal'
  | 'activity'
  | 'cpu'
  | 'memory'
  | 'clock'
  | 'network'
  | 'crown'
  | 'logout'
  | 'external'
  | 'chevron-right'
  | 'eye'
  | 'eye-off'
  | 'dice'
  | 'copy'
  | 'sync'
  | 'sparkle'
  | 'cloud'
  | 'stack'
  | 'node'
  | 'container'
  | 'x'
  | 'pause'
  | 'more'
  | 'chevron'
  | 'globe'
  | 'archive'
  | 'proxy';

// Every path is drawn on a 24x24 grid with round caps and no fill, so they
// share one optical weight regardless of the size they are rendered at.
const PATHS: Record<IconName, JSX.Element> = {
  overview: (
    <>
      <rect x="3" y="3" width="7" height="8" rx="1.5" />
      <rect x="14" y="3" width="7" height="5" rx="1.5" />
      <rect x="14" y="11" width="7" height="10" rx="1.5" />
      <rect x="3" y="14" width="7" height="7" rx="1.5" />
    </>
  ),
  nodes: (
    <>
      <rect x="3" y="4" width="18" height="7" rx="2" />
      <rect x="3" y="13" width="18" height="7" rx="2" />
      <path d="M7 7.5h.01M7 16.5h.01" />
    </>
  ),
  stacks: (
    <>
      <path d="M12 3 3 7.5 12 12l9-4.5L12 3Z" />
      <path d="m3 12.5 9 4.5 9-4.5" />
      <path d="m3 17 9 4.5L21 17" />
    </>
  ),
  instances: (
    <>
      <path d="M21 8.5v7a2 2 0 0 1-1.05 1.76l-7 3.5a2 2 0 0 1-1.9 0l-7-3.5A2 2 0 0 1 3 15.5v-7a2 2 0 0 1 1.05-1.76l7-3.5a2 2 0 0 1 1.9 0l7 3.5A2 2 0 0 1 21 8.5Z" />
      <path d="m3.3 7.5 8.7 4.4 8.7-4.4M12 21v-9.1" />
    </>
  ),
  secrets: (
    <>
      <path d="M15.5 8.5a3.5 3.5 0 1 1-3.4-3.5" />
      <path d="M21 3l-7.6 7.6M18 6l2 2M15.5 8.5l2 2" />
      <path d="M11 14v6M8 20h6" />
    </>
  ),
  drives: (
    <>
      <ellipse cx="12" cy="5.5" rx="8" ry="2.8" />
      <path d="M4 5.5v13c0 1.55 3.58 2.8 8 2.8s8-1.25 8-2.8v-13" />
      <path d="M4 12c0 1.55 3.58 2.8 8 2.8s8-1.25 8-2.8" />
    </>
  ),
  settings: (
    <>
      <path d="M4 6h10M18 6h2M4 12h2M10 12h10M4 18h8M16 18h4" />
      <circle cx="16" cy="6" r="2" />
      <circle cx="8" cy="12" r="2" />
      <circle cx="14" cy="18" r="2" />
    </>
  ),
  certificate: (
    <>
      <path d="M12 3 4.5 6v5.5c0 4.4 3.1 8.4 7.5 9.5 4.4-1.1 7.5-5.1 7.5-9.5V6L12 3Z" />
      <path d="m9 12 2 2 4-4" />
    </>
  ),
  menu: <path d="M4 6h16M4 12h16M4 18h16" />,
  close: <path d="M6 6l12 12M18 6 6 18" />,
  plus: <path d="M12 5v14M5 12h14" />,
  trash: (
    <>
      <path d="M4 7h16M10 4h4M6 7l1 13h10l1-13" />
      <path d="M10 11v6M14 11v6" />
    </>
  ),
  refresh: (
    <>
      <path d="M20 12a8 8 0 1 1-2.6-5.9" />
      <path d="M20 4v5h-5" />
    </>
  ),
  play: <path d="M7 4.5 19 12 7 19.5v-15Z" />,
  stop: <rect x="6" y="6" width="12" height="12" rx="2" />,
  restart: (
    <>
      <path d="M4 12a8 8 0 0 1 13.7-5.6L20 8" />
      <path d="M20 4v4h-4" />
      <path d="M20 12a8 8 0 0 1-13.7 5.6L4 16" />
      <path d="M4 20v-4h4" />
    </>
  ),
  move: (
    <>
      <path d="M12 3v18M3 12h18" />
      <path d="m9 6 3-3 3 3M9 18l3 3 3-3M6 9l-3 3 3 3M18 9l3 3-3 3" />
    </>
  ),
  edit: (
    <>
      <path d="M12 20h9" />
      <path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7.5 18.5 3 20l1.5-4.5 12-12Z" />
    </>
  ),
  upload: (
    <>
      <path d="M4 15v3a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-3" />
      <path d="M12 4v12M8 8l4-4 4 4" />
    </>
  ),
  download: (
    <>
      <path d="M4 15v3a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-3" />
      <path d="M12 16V4M8 12l4 4 4-4" />
    </>
  ),
  check: <path d="m5 13 4 4L19 7" />,
  'check-circle': (
    <>
      <circle cx="12" cy="12" r="9" />
      <path d="m8.5 12 2.5 2.5 4.5-5" />
    </>
  ),
  alert: (
    <>
      <path d="M10.3 4.3 2.6 17.5A2 2 0 0 0 4.3 20.5h15.4a2 2 0 0 0 1.7-3L13.7 4.3a2 2 0 0 0-3.4 0Z" />
      <path d="M12 9.5v4M12 17h.01" />
    </>
  ),
  'alert-circle': (
    <>
      <circle cx="12" cy="12" r="9" />
      <path d="M12 7.5v5M12 16h.01" />
    </>
  ),
  info: (
    <>
      <circle cx="12" cy="12" r="9" />
      <path d="M12 11v5M12 8h.01" />
    </>
  ),
  search: (
    <>
      <circle cx="11" cy="11" r="6.5" />
      <path d="m20 20-4.3-4.3" />
    </>
  ),
  filter: <path d="M3 5h18l-7 8v6l-4 2v-8L3 5Z" />,
  sun: (
    <>
      <circle cx="12" cy="12" r="4" />
      <path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4" />
    </>
  ),
  moon: <path d="M20 14.5A8.5 8.5 0 0 1 9.5 4a8.5 8.5 0 1 0 10.5 10.5Z" />,
  system: (
    <>
      <rect x="3" y="4" width="18" height="12" rx="2" />
      <path d="M8 20h8M12 16v4" />
    </>
  ),
  key: (
    <>
      <circle cx="8" cy="15" r="4" />
      <path d="m11 12 8-8M17 6l2 2M14.5 8.5l2 2" />
    </>
  ),
  lock: (
    <>
      <rect x="4" y="10" width="16" height="11" rx="2" />
      <path d="M8 10V7a4 4 0 0 1 8 0v3" />
    </>
  ),
  file: (
    <>
      <path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8l-5-5Z" />
      <path d="M14 3v5h5M9 13h6M9 17h6" />
    </>
  ),
  terminal: (
    <>
      <rect x="3" y="4" width="18" height="16" rx="2" />
      <path d="m7 9 3 3-3 3M13 15h4" />
    </>
  ),
  activity: <path d="M3 12h4l3 8 4-16 3 8h4" />,
  cpu: (
    <>
      <rect x="6" y="6" width="12" height="12" rx="2" />
      <rect x="9.5" y="9.5" width="5" height="5" rx="1" />
      <path d="M9 3v3M15 3v3M9 18v3M15 18v3M3 9h3M3 15h3M18 9h3M18 15h3" />
    </>
  ),
  memory: (
    <>
      <rect x="3" y="7" width="18" height="10" rx="2" />
      <path d="M7 17v2M12 17v2M17 17v2M7 11v2M12 11v2M17 11v2" />
    </>
  ),
  clock: (
    <>
      <circle cx="12" cy="12" r="9" />
      <path d="M12 7v5.2l3.2 2" />
    </>
  ),
  network: (
    <>
      <circle cx="12" cy="5" r="2.5" />
      <circle cx="5" cy="19" r="2.5" />
      <circle cx="19" cy="19" r="2.5" />
      <path d="M12 7.5v4M12 11.5 6.5 17M12 11.5 17.5 17" />
    </>
  ),
  crown: <path d="M4 18h16l1-11-5.5 4L12 4 8.5 11 3 7l1 11Z" />,
  logout: (
    <>
      <path d="M14 4h3a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2h-3" />
      <path d="M10 16 6 12l4-4M6 12h9" />
    </>
  ),
  external: (
    <>
      <path d="M14 4h6v6" />
      <path d="M20 4 11 13" />
      <path d="M18 14v4a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4" />
    </>
  ),
  'chevron-right': <path d="m9 5 7 7-7 7" />,
  eye: (
    <>
      <path d="M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12Z" />
      <circle cx="12" cy="12" r="3" />
    </>
  ),
  'eye-off': (
    <>
      <path d="M4 4l16 16" />
      <path d="M9.9 5.9A9.6 9.6 0 0 1 12 5.5c6 0 9.5 6.5 9.5 6.5a17 17 0 0 1-3.3 4.1M6.6 7.9A17 17 0 0 0 2.5 12S6 18.5 12 18.5a9.4 9.4 0 0 0 3.4-.6" />
      <path d="M9.9 10.1a3 3 0 0 0 4.1 4.2" />
    </>
  ),
  dice: (
    <>
      <rect x="3" y="3" width="18" height="18" rx="3" />
      <path d="M8.5 8.5h.01M15.5 8.5h.01M12 12h.01M8.5 15.5h.01M15.5 15.5h.01" />
    </>
  ),
  copy: (
    <>
      <rect x="9" y="9" width="12" height="12" rx="2" />
      <path d="M5 15H4a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v1" />
    </>
  ),
  sync: (
    <>
      <path d="M21 12a9 9 0 0 1-14.5 7.1" />
      <path d="M3 12A9 9 0 0 1 17.5 4.9" />
      <path d="M17 2v4h-4M7 22v-4h4" />
    </>
  ),
  sparkle: (
    <>
      <path d="M12 3l1.8 5.2L19 10l-5.2 1.8L12 17l-1.8-5.2L5 10l5.2-1.8L12 3Z" />
      <path d="M19 16l.7 2.3L22 19l-2.3.7L19 22l-.7-2.3L16 19l2.3-.7L19 16Z" />
    </>
  ),
  cloud: <path d="M7 19a4.5 4.5 0 0 1-.6-8.96A6 6 0 0 1 17.7 9.2 4 4 0 0 1 17 19H7Z" />,
  // The redesign's glyphs (design-ref/ds/clouduler/components/bundle.js).
  // `x` and `pause` are the circled forms the status badges use; `check` stays
  // the bare tick, so the circled check is `check-circle`.
  stack: (
    <>
      <path d="M12 3 3 8l9 5 9-5-9-5Z" />
      <path d="M3 13l9 5 9-5" />
    </>
  ),
  node: (
    <>
      <path d="M4 4h16v6H4z" />
      <path d="M4 14h16v6H4z" />
      <path d="M8 7h.01M8 17h.01" />
    </>
  ),
  container: (
    <>
      <path d="M21 8 12 3 3 8v8l9 5 9-5V8Z" />
      <path d="M3 8l9 5 9-5M12 13v8" />
    </>
  ),
  x: (
    <>
      <path d="M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18Z" />
      <path d="M9 9l6 6M15 9l-6 6" />
    </>
  ),
  pause: (
    <>
      <path d="M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18Z" />
      <path d="M10 9v6M14 9v6" />
    </>
  ),
  more: <path d="M5 12h.01M12 12h.01M19 12h.01" />,
  chevron: <path d="M9 6l6 6-6 6" />,
  globe: (
    <>
      <path d="M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18Z" />
      <path d="M3 12h18M12 3c3 3 3 15 0 18M12 3c-3 3-3 15 0 18" />
    </>
  ),
  archive: (
    <>
      <path d="M3 5h18v4H3z" />
      <path d="M5 9v10h14V9M10 13h4" />
    </>
  ),
  proxy: (
    <>
      <path d="M4 8h13M13 4l4 4-4 4" />
      <path d="M20 16H7M11 12l-4 4 4 4" />
    </>
  ),
};

export interface IconProps extends Omit<SVGProps<SVGSVGElement>, 'name'> {
  name: IconName;
  /** Size in px. Defaults to the CSS-controlled 1.15em box. */
  size?: number;
  /** Visible label; without one the icon is hidden from assistive tech. */
  title?: string;
}

export function Icon({ name, size, title, className, style, ...rest }: IconProps) {
  return (
    <svg
      className={className ? `icon ${className}` : 'icon'}
      width={size}
      height={size}
      style={size ? { width: size, height: size, ...style } : style}
      viewBox="0 0 24 24"
      fill={name === 'play' || name === 'stop' || name === 'filter' || name === 'crown' ? 'currentColor' : 'none'}
      stroke="currentColor"
      strokeWidth={1.75}
      strokeLinecap="round"
      strokeLinejoin="round"
      role={title ? 'img' : undefined}
      aria-hidden={title ? undefined : true}
      focusable="false"
      {...rest}
    >
      {title && <title>{title}</title>}
      {PATHS[name]}
    </svg>
  );
}
