import type { ReactNode } from 'react';

/** Matches a real ESC (or its printable "␛" stand-in) followed by an SGR code. */
const SGR = /[\x1b␛]\[([\d;]*)m/g;
/** Any other CSI sequence (cursor moves, erase...) is dropped, not rendered. */
const OTHER_CSI = /[\x1b␛]\[[\d;?]*[A-Za-ln-z]/g;
/** A producer's own leading timestamp (ISO-8601 or `10/07/2026, 9:01:28 PM`),
 *  possibly behind an ANSI colour; clouduler already shows its own. */
const LEADING_TS =
  /^(?:[\x1b␛]\[[\d;]*m)*\s*(?:\[?\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?\]?|\[?\d{1,2}\/\d{1,2}\/\d{4},? \d{1,2}:\d{2}:\d{2}(?: [AP]M)?\]?)\s*/;

const NAMES = ['black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white'];

interface Style {
  fg?: string;
  bold?: boolean;
  dim?: boolean;
}

function applyCodes(prev: Style, codes: number[]): Style {
  let s = { ...prev };
  for (const c of codes) {
    if (c === 0) s = {};
    else if (c === 1) s.bold = true;
    else if (c === 2) s.dim = true;
    else if (c === 22) s.bold = s.dim = false;
    else if (c >= 30 && c <= 37) s.fg = NAMES[c - 30];
    else if (c >= 90 && c <= 97) s.fg = NAMES[c - 90];
    else if (c === 39) s.fg = undefined;
  }
  return s;
}

/** Colour implied by a level word in the line's leading prefix, used only when
 *  the line carries no colour of its own. */
function prefixColor(plain: string): string | undefined {
  const raw = plain.slice(0, 80);
  const head = raw.toUpperCase();
  if (/\b(ERROR|ERR|FATAL|CRITICAL|PANIC)\b/.test(head)) return 'red';
  if (/\b(WARN|WARNING)\b/.test(head)) return 'yellow';
  if (/\b(INFO|LOG|NOTICE)\b/.test(raw)) return 'green';
  if (/\b(DEBUG|TRACE|VERBOSE)\b/.test(raw)) return 'cyan';
  return undefined;
}

/** Renders a log message: drops a leading producer timestamp, turns ANSI SGR
 *  colours into styled spans, and falls back to a colour derived from the
 *  level prefix (`ERROR`, `WARN`, `[Nest] ... LOG`...) when the line has none. */
export function LogMessage({ text }: { text: string }): ReactNode {
  const stripped = text.replace(LEADING_TS, '').replace(OTHER_CSI, '');
  const hasColour = /[\x1b␛]\[[\d;]*m/.test(stripped);
  const parts: ReactNode[] = [];
  let style: Style = {};
  let last = 0;
  let plain = '';
  const push = (chunk: string, st: Style) => {
    if (!chunk) return;
    plain += chunk;
    const cls = ['ansi', st.fg && `ansi-${st.fg}`, st.bold && 'ansi-bold', st.dim && 'ansi-dim']
      .filter(Boolean)
      .join(' ');
    parts.push(
      st.fg || st.bold || st.dim ? (
        <span key={parts.length} className={cls}>
          {chunk}
        </span>
      ) : (
        chunk
      ),
    );
  };
  for (const m of stripped.matchAll(SGR)) {
    push(stripped.slice(last, m.index), style);
    const codes = m[1] === '' ? [0] : m[1].split(';').map((n) => Number(n) || 0);
    style = applyCodes(style, codes);
    last = m.index + m[0].length;
  }
  push(stripped.slice(last), style);

  if (!hasColour) {
    const fg = prefixColor(stripped);
    if (fg) return <span className={`ansi ansi-${fg}`}>{stripped}</span>;
  }
  return <>{parts}</>;
}
