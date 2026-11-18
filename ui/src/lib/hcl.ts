// Small StreamLanguage tokenizer for HCL / Terraform. There is no first-party
// CodeMirror HCL package, and the editor only needs lexical highlighting.

import { LanguageSupport, StreamLanguage, type StringStream } from '@codemirror/language';
import { tags as t } from '@lezer/highlight';

const BLOCK_KEYWORDS = new Set([
  'resource', 'data', 'variable', 'output', 'locals', 'module', 'provider', 'terraform', 'moved', 'import', 'check', 'removed',
]);
const KEYWORDS = new Set(['for', 'in', 'if', 'else', 'endif', 'endfor']);
const ATOMS = new Set(['true', 'false', 'null']);
const ROOTS = new Set(['var', 'local', 'module', 'data', 'each', 'count', 'path', 'self', 'terraform']);

interface HclState {
  /** Inside a /* block comment. */
  comment: boolean;
  /** Inside a double-quoted string that continues on this line. */
  str: boolean;
  /** Heredoc terminator while inside one. */
  heredoc: { id: string; indented: boolean } | null;
}

/** Consume a ${...} or %{...} sequence (the opening two chars are already at the cursor). */
function interpolation(stream: StringStream) {
  stream.next();
  stream.next();
  let depth = 1;
  let quote = false;
  while (depth > 0 && !stream.eol()) {
    const c = stream.next();
    if (c === '\\' && quote) stream.next();
    else if (c === '"') quote = !quote;
    else if (!quote && c === '{') depth++;
    else if (!quote && c === '}') depth--;
  }
}

/** Consume string content up to a closing quote (when not a heredoc) or an interpolation. */
function stringBody(stream: StringStream, state: HclState): string {
  if (stream.match(/^[$%]\{/, false)) {
    interpolation(stream);
    return 'interp';
  }
  while (!stream.eol()) {
    if (stream.match(/^[$%]\{/, false)) break;
    if (stream.match(/^[$%]{2}\{/)) continue; // escaped `${`
    const c = stream.next();
    if (c === '\\' && !state.heredoc) stream.next();
    else if (c === '"' && !state.heredoc) {
      state.str = false;
      break;
    }
  }
  return 'string';
}

const hclParser = StreamLanguage.define<HclState>({
  name: 'hcl',
  startState: () => ({ comment: false, str: false, heredoc: null }),
  copyState: (s) => ({ ...s, heredoc: s.heredoc && { ...s.heredoc } }),
  token(stream, state): string | null {
    if (state.comment) {
      while (!stream.eol()) {
        if (stream.match('*/')) {
          state.comment = false;
          break;
        }
        stream.next();
      }
      return 'comment';
    }
    if (state.heredoc) {
      if (stream.sol()) {
        const { id, indented } = state.heredoc;
        const line = stream.string;
        if ((indented ? line.trim() : line.trimEnd()) === id) {
          stream.skipToEnd();
          state.heredoc = null;
          return 'heredocMark';
        }
      }
      return stringBody(stream, state);
    }
    if (state.str) {
      if (stream.sol()) state.str = false; // strings never span lines
      else return stringBody(stream, state);
    }
    if (stream.eatSpace()) return null;

    if (stream.match('/*')) {
      state.comment = true;
      while (!stream.eol()) {
        if (stream.match('*/')) {
          state.comment = false;
          break;
        }
        stream.next();
      }
      return 'comment';
    }
    if (stream.match('#') || stream.match('//')) {
      stream.skipToEnd();
      return 'comment';
    }
    const here = stream.match(/^<<(-?)([A-Za-z_][\w-]*)/);
    if (here && Array.isArray(here)) {
      state.heredoc = { id: here[2], indented: here[1] === '-' };
      return 'heredocMark';
    }
    if (stream.eat('"')) {
      state.str = true;
      return stringBody(stream, state);
    }
    if (stream.match(/^\d+(\.\d+)?([eE][+-]?\d+)?/)) return 'number';

    if (stream.match(/^[A-Za-z_][\w-]*/)) {
      const word = stream.current();
      const first = stream.string.slice(0, stream.start).trim() === '';
      const rest = stream.string.slice(stream.pos);
      if (first && /^\s*("|\{|[A-Za-z_])/.test(rest) && !/^\s*=/.test(rest)) {
        return BLOCK_KEYWORDS.has(word) ? 'keyword' : 'blockType';
      }
      if (ATOMS.has(word)) return 'atom';
      if (KEYWORDS.has(word)) return 'keyword';
      if (/^\s*=(?![=>])/.test(rest) && !/^\.|^\[/.test(rest)) return 'attr';
      if (rest.startsWith('(')) return 'fn';
      if (ROOTS.has(word) && rest.startsWith('.')) return 'root';
      return 'variable';
    }
    if (stream.match(/^(=>|==|!=|<=|>=|&&|\|\||\.\.\.|[-+*/%<>!?:=.,])/)) return 'operator';
    if (stream.match(/^[{}[\]()]/)) return 'bracket';
    stream.next();
    return null;
  },
  languageData: {
    commentTokens: { line: '#', block: { open: '/*', close: '*/' } },
    closeBrackets: { brackets: ['(', '[', '{', '"'] },
    indentOnInput: /^\s*[}\])]$/,
  },
  tokenTable: {
    interp: t.meta,
    heredocMark: t.meta,
    blockType: t.typeName,
    attr: t.propertyName,
    fn: t.function(t.variableName),
    root: t.standard(t.variableName),
    variable: t.variableName,
    comment: t.comment,
    string: t.string,
    number: t.number,
    atom: t.atom,
    keyword: t.keyword,
    operator: t.operator,
    bracket: t.bracket,
  },
});

export function hcl(): LanguageSupport {
  return new LanguageSupport(hclParser);
}
