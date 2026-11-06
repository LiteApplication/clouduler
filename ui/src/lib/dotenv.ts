// A small .env parser for importing a service's `env_file` in the stack
// editor. Runs only in the browser; values are returned to the caller and
// never logged.

export interface DotenvEntry {
  key: string;
  value: string;
  /** 1-based line the (last) definition starts on. */
  line: number;
}

export interface DotenvIssue {
  line: number;
  message: string;
}

export interface DotenvResult {
  /** One entry per key, in first-seen order, holding the last value. */
  entries: DotenvEntry[];
  /** Lines that could not be read. Nothing on them is imported. */
  errors: DotenvIssue[];
  /** Duplicate keys: the last definition wins. */
  warnings: DotenvIssue[];
}

const KEY = /^[A-Za-z_][A-Za-z0-9_.-]*/;
const ESCAPES: Record<string, string> = { n: '\n', r: '\r', t: '\t', '\\': '\\', '"': '"', $: '$' };

/** Parses dotenv text: `KEY=value`, `export KEY=value`, `#` comment lines,
 *  single quotes (literal), double quotes (with \n \r \t \\ \" escapes), and
 *  quoted values that span lines. Unquoted values lose a trailing ` # comment`.
 *  Variables (`${X}`) are not expanded. */
export function parseDotenv(text: string): DotenvResult {
  const lines = text.replace(/^\uFEFF/, '').split(/\r?\n/);
  const order: string[] = [];
  const byKey = new Map<string, DotenvEntry>();
  const errors: DotenvIssue[] = [];
  const warnings: DotenvIssue[] = [];

  for (let i = 0; i < lines.length; i++) {
    const lineNo = i + 1;
    let line = lines[i].trimStart();
    if (!line || line.startsWith('#')) continue;
    if (/^export\s+/.test(line)) line = line.replace(/^export\s+/, '');
    const km = KEY.exec(line);
    if (!km) {
      errors.push({ line: lineNo, message: 'Expected KEY=value.' });
      continue;
    }
    const key = km[0];
    let rest = line.slice(key.length).trimStart();
    if (!rest.startsWith('=')) {
      errors.push({ line: lineNo, message: `"${key}" has no "=". A bare name that passes a host variable through is not supported.` });
      continue;
    }
    rest = rest.slice(1).trimStart();

    let value: string;
    const quote = rest[0];
    if (quote === '"' || quote === "'") {
      // Scan to the closing quote, across lines if needed.
      let buf = '';
      let cur = rest.slice(1);
      let closed = false;
      let tail = '';
      let j = i;
      for (;;) {
        let k = 0;
        while (k < cur.length) {
          const c = cur[k];
          if (quote === '"' && c === '\\' && k + 1 < cur.length) {
            const n = cur[k + 1];
            buf += n in ESCAPES ? ESCAPES[n] : c + n;
            k += 2;
          } else if (c === quote) {
            closed = true;
            tail = cur.slice(k + 1);
            break;
          } else {
            buf += c;
            k++;
          }
        }
        if (closed || j + 1 >= lines.length) break;
        buf += '\n';
        j++;
        cur = lines[j];
      }
      if (!closed) {
        errors.push({ line: lineNo, message: `Unterminated ${quote === '"' ? 'double' : 'single'} quote.` });
        break;
      }
      const t = tail.trim();
      if (t && !t.startsWith('#')) {
        errors.push({ line: lineNo, message: 'Unexpected text after the closing quote.' });
        i = j;
        continue;
      }
      value = buf;
      i = j;
    } else {
      value = rest.replace(/\s+#.*$/, '').trimEnd();
      if (value.startsWith('#')) value = '';
    }

    const prev = byKey.get(key);
    if (prev) {
      warnings.push({ line: lineNo, message: `${key} is defined again (first on line ${prev.line}). The last value wins.` });
      prev.value = value;
      prev.line = lineNo;
    } else {
      const entry = { key, value, line: lineNo };
      byKey.set(key, entry);
      order.push(key);
    }
  }
  return { entries: order.map((k) => byKey.get(k)!), errors, warnings };
}
