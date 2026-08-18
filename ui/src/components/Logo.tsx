// The clouduler mark, inline so it renders before any asset request resolves
// and so it can pick up theme colors. The same artwork lives in
// public/icon.svg, which is what the manifest and favicon point at.

export function Logo({ size = 32, className }: { size?: number; className?: string }) {
  return (
    <svg
      className={className}
      width={size}
      height={size}
      viewBox="0 0 512 512"
      role="img"
      aria-label="clouduler"
    >
      <defs>
        <linearGradient id="cl-plate" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" stopColor="#0a1420" />
          <stop offset="0.55" stopColor="#01080d" />
          <stop offset="1" stopColor="#1a0620" />
        </linearGradient>
        <radialGradient id="cl-glow" cx="0.5" cy="0.62" r="0.62">
          <stop offset="0" stopColor="#820e8e" stopOpacity="0.85" />
          <stop offset="0.6" stopColor="#ec39a2" stopOpacity="0.18" />
          <stop offset="1" stopColor="#ec39a2" stopOpacity="0" />
        </radialGradient>
        <linearGradient id="cl-cloud" x1="0.1" y1="0" x2="0.9" y2="1">
          <stop offset="0" stopColor="#ddecfc" />
          <stop offset="1" stopColor="#86baf3" />
        </linearGradient>
        <linearGradient id="cl-edge" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" stopColor="#ec39a2" />
          <stop offset="1" stopColor="#820e8e" />
        </linearGradient>
      </defs>
      <rect width="512" height="512" rx="112" fill="url(#cl-plate)" />
      <rect width="512" height="512" rx="112" fill="url(#cl-glow)" />
      <path
        fill="url(#cl-cloud)"
        d="M150 372c-42 0-76-34-76-76 0-38 28-70 65-75 12-46 54-79 103-79 51 0 94 36 104 84 40 3 72 37 72 78 0 43-35 78-78 78z"
      />
      <g stroke="url(#cl-edge)" strokeWidth="14" strokeLinecap="round" fill="none">
        <path d="M256 210v52" />
        <path d="M256 262 190 306" />
        <path d="M256 262 322 306" />
      </g>
      <g fill="#01080d">
        <circle cx="256" cy="198" r="28" />
        <circle cx="186" cy="312" r="25" />
        <circle cx="326" cy="312" r="25" />
      </g>
      <circle cx="256" cy="198" r="13" fill="#ec39a2" />
      <circle cx="186" cy="312" r="11" fill="#820e8e" />
      <circle cx="326" cy="312" r="11" fill="#820e8e" />
    </svg>
  );
}
