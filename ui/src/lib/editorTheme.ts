// CodeMirror theme and YAML highlighting for the spec editor, drawn entirely
// from the design-system tokens so it follows dark/light with the page.

import { HighlightStyle } from '@codemirror/language';
import { StateEffect, StateField, RangeSet } from '@codemirror/state';
import { Decoration, EditorView, GutterMarker, gutter, type DecorationSet } from '@codemirror/view';
import { tags as t } from '@lezer/highlight';

export const specHighlightStyle = HighlightStyle.define([
  { tag: [t.propertyName, t.definition(t.propertyName)], color: 'var(--brand)' },
  { tag: [t.string, t.special(t.string)], color: 'var(--ok)' },
  { tag: [t.number, t.bool, t.null, t.atom], color: 'var(--pop)' },
  { tag: t.comment, color: 'var(--ink-muted)', fontStyle: 'italic' },
  { tag: [t.punctuation, t.separator, t.squareBracket, t.brace], color: 'var(--line-strong)' },
  { tag: [t.labelName, t.typeName, t.meta, t.keyword], color: 'var(--warn)' },
  { tag: t.invalid, color: 'var(--danger)' },
]);

export const specTheme = EditorView.theme({
  '&': {
    color: 'var(--ink)',
    backgroundColor: 'var(--surface-2)',
    border: '1px solid var(--line)',
    borderRadius: 'var(--radius-md)',
    fontSize: '13px',
  },
  '&.cm-focused': { outline: '2px solid var(--brand)', outlineOffset: '2px' },
  '.cm-scroller': { fontFamily: 'var(--font-mono)', lineHeight: '20px', borderRadius: 'var(--radius-md)' },
  '.cm-content': { padding: '10px 0', caretColor: 'var(--brand)' },
  '.cm-cursor': { borderLeftColor: 'var(--brand)' },
  '.cm-gutters': { backgroundColor: 'var(--surface-2)', color: 'var(--ink-muted)', border: 'none' },
  '.cm-activeLine': { backgroundColor: 'var(--surface-1)' },
  '.cm-activeLineGutter': { backgroundColor: 'var(--surface-1)' },
  '&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection': {
    backgroundColor: 'var(--brand-bg) !important',
  },
  '.cm-tooltip': {
    backgroundColor: 'var(--surface-1)',
    border: '1px solid var(--line-strong)',
    borderRadius: 'var(--radius-md)',
    color: 'var(--ink)',
  },
  '.cm-tooltip.cm-tooltip-autocomplete > ul > li[aria-selected]': {
    backgroundColor: 'var(--brand-bg)',
    color: 'var(--brand)',
  },
  '.cm-object-link': {
    cursor: 'pointer',
    textDecoration: 'underline',
    textDecorationColor: 'var(--brand)',
    textDecorationStyle: 'dotted',
    textUnderlineOffset: '2px',
  },
  '.cm-error-line': {
    textDecoration: 'underline wavy var(--danger)',
    textUnderlineOffset: '3px',
  },
  '.cm-error-gutter': { width: '18px' },
  '.cm-error-mark': { color: 'var(--danger)', fontWeight: '700', textAlign: 'center' },
});

/** 1-based line numbers the editor should flag as invalid. */
export const setErrorLines = StateEffect.define<number[]>();

const errorLineDeco = Decoration.line({ class: 'cm-error-line' });

class ErrorMarker extends GutterMarker {
  toDOM() {
    const el = document.createElement('span');
    el.className = 'cm-error-mark';
    el.setAttribute('aria-label', 'Error on this line');
    el.textContent = '×';
    return el;
  }
}
const errorMarker = new ErrorMarker();

const errorLineGutterField = StateField.define<RangeSet<GutterMarker>>({
  create: () => RangeSet.empty,
  update(set, tr) {
    for (const e of tr.effects) {
      if (!e.is(setErrorLines)) continue;
      const doc = tr.state.doc;
      return RangeSet.of(
        e.value
          .filter((n) => n >= 1 && n <= doc.lines)
          .sort((a, b) => a - b)
          .map((n) => errorMarker.range(doc.line(n).from)),
      );
    }
    return set.map(tr.changes);
  },
});

/** Decorations and gutter mark for the lines set with `setErrorLines`. */
export const errorLines = [
  StateField.define<DecorationSet>({
    create: () => Decoration.none,
    update(deco, tr) {
      deco = deco.map(tr.changes);
      for (const e of tr.effects) {
        if (!e.is(setErrorLines)) continue;
        const doc = tr.state.doc;
        deco = Decoration.set(
          e.value
            .filter((n) => n >= 1 && n <= doc.lines)
            .map((n) => errorLineDeco.range(doc.line(n).from))
            .sort((a, b) => a.from - b.from),
        );
      }
      return deco;
    },
    provide: (f) => EditorView.decorations.from(f),
  }),
  errorLineGutterField,
  gutter({
    class: 'cm-error-gutter',
    markers: (view) => {
      const set = view.state.field(errorLineGutterField, false);
      return set ?? RangeSet.empty;
    },
  }),
];

