// The clouduler mark, inline so it renders before any asset request resolves.
// The same artwork lives in public/icon.svg, which is what the manifest and
// favicon point at; keep the two in step.

export function Logo({ size = 32, className }: { size?: number; className?: string }) {
  return (
    <svg
      className={className}
      width={size}
      height={size}
      viewBox="0 0 64 64"
      role="img"
      aria-label="clouduler"
    >
      <rect width="64" height="64" rx="16" fill="#150f1f" />
      <rect x="1.5" y="1.5" width="61" height="61" rx="14.5" fill="none" stroke="#3a2f57" strokeWidth="3" />
      <g transform="translate(-2 1)">
        <g fill="#ff5cae">
          <circle cx="23" cy="37" r="9" />
          <circle cx="33" cy="30" r="12" />
          <circle cx="45" cy="37" r="9" />
          <rect x="23" y="37" width="22" height="9" />
        </g>
        <g stroke="#150f1f" strokeWidth="2" strokeLinecap="round">
          <line x1="34" y1="29" x2="25" y2="40" />
          <line x1="34" y1="29" x2="43" y2="40" />
          <line x1="25" y1="40" x2="43" y2="40" />
        </g>
        <circle cx="25" cy="40" r="3.5" fill="#150f1f" />
        <circle cx="43" cy="40" r="3.5" fill="#150f1f" />
        <circle cx="34" cy="29" r="5" fill="#c8f542" stroke="#150f1f" strokeWidth="2" />
      </g>
    </svg>
  );
}
