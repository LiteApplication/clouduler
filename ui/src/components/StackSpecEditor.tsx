import { useEffect, useRef } from 'react';
import { useNavigate } from 'react-router-dom';
import { autocompletion, closeBrackets, completionKeymap } from '@codemirror/autocomplete';
import { defaultKeymap, history, historyKeymap } from '@codemirror/commands';
import { yaml } from '@codemirror/lang-yaml';
import { HighlightStyle, syntaxHighlighting } from '@codemirror/language';
import { Compartment, EditorState, Facet } from '@codemirror/state';
import { Decoration, type DecorationSet, EditorView, keymap, MatchDecorator, ViewPlugin, type ViewUpdate } from '@codemirror/view';
import { tags as t } from '@lezer/highlight';
import { buildObjectIndex, objectRouteFor, emptyKnownNames, stackSpecCompletionSource, type KnownNames } from '../lib/stackSchema';

// @codemirror/language's defaultHighlightStyle hard-codes light-mode colors,
// so yaml keys/strings/comments went unreadable on the dark theme. This maps
// tags to the same CSS variables the rest of the theme uses, so it follows
// light/dark (and the color-mix'd variants) automatically.
const stackHighlightStyle = HighlightStyle.define([
  { tag: t.propertyName, color: 'var(--primary)' },
  { tag: [t.keyword, t.bool, t.atom, t.null], color: 'var(--accent)' },
  { tag: [t.string, t.special(t.string)], color: 'var(--good)' },
  { tag: t.number, color: 'var(--secondary)' },
  { tag: t.comment, color: 'var(--text-faint)', fontStyle: 'italic' },
  { tag: [t.meta, t.punctuation], color: 'var(--text-dim)' },
  { tag: t.invalid, color: 'var(--bad)' },
]);

const theme = EditorView.theme({
  '&': {
    color: 'var(--text)',
    backgroundColor: 'var(--surface-2)',
    border: '1px solid var(--border)',
    borderRadius: 'var(--radius-sm)',
  },
  '&.cm-focused': {
    outline: 'none',
    borderColor: 'var(--primary)',
    boxShadow: 'var(--glow)',
  },
  '.cm-content': {
    fontFamily: 'var(--mono)',
    fontSize: '13px',
    padding: '10px 0',
  },
  '.cm-gutters': {
    backgroundColor: 'var(--surface-2)',
    color: 'var(--text-faint)',
    border: 'none',
  },
  '.cm-activeLine': { backgroundColor: 'color-mix(in srgb, var(--primary) 6%, transparent)' },
  '.cm-activeLineGutter': { backgroundColor: 'color-mix(in srgb, var(--primary) 6%, transparent)' },
  '.cm-selectionBackground': { backgroundColor: 'color-mix(in srgb, var(--primary) 25%, transparent) !important' },
  '.cm-cursor': { borderLeftColor: 'var(--text)' },
  '.cm-tooltip': {
    backgroundColor: 'var(--surface)',
    border: '1px solid var(--border)',
    color: 'var(--text)',
  },
  '.cm-tooltip.cm-tooltip-autocomplete > ul > li[aria-selected]': {
    backgroundColor: 'color-mix(in srgb, var(--accent) 18%, transparent)',
    color: 'var(--text)',
  },
  // Every clickable object reference gets a permanent, subtle dotted
  // underline so ctrl/cmd+click is discoverable without first holding the
  // modifier — tracking live modifier state to gate the style would need a
  // document-wide keydown/keyup listener for a purely cosmetic difference.
  '.cm-object-link': {
    cursor: 'pointer',
    textDecoration: 'underline',
    textDecorationColor: 'var(--accent)',
    textDecorationStyle: 'dotted',
    textUnderlineOffset: '2px',
  },
});

const objectLinkMark = Decoration.mark({ class: 'cm-object-link' });

function buildMatcher(known: KnownNames): MatchDecorator {
  const index = buildObjectIndex(known);
  return new MatchDecorator({
    regexp: /[A-Za-z0-9][A-Za-z0-9._-]*/g,
    decorate: (add, from, to, match) => {
      if (index.has(match[0])) add(from, to, objectLinkMark);
    },
  });
}

// Holds the live known-names set inside editor state (via a per-view
// Compartment) so the decoration ViewPlugin reacts to cluster-state updates
// without tearing down and recreating the whole EditorView.
const knownNamesFacet = Facet.define<KnownNames, KnownNames>({
  combine: (values) => values[values.length - 1] ?? emptyKnownNames,
});

const objectLinkPlugin = ViewPlugin.fromClass(
  class {
    matcher: MatchDecorator;
    decorations: DecorationSet;
    known: KnownNames;

    constructor(view: EditorView) {
      this.known = view.state.facet(knownNamesFacet);
      this.matcher = buildMatcher(this.known);
      this.decorations = this.matcher.createDeco(view);
    }

    update(update: ViewUpdate) {
      const known = update.state.facet(knownNamesFacet);
      if (known !== this.known) {
        this.known = known;
        this.matcher = buildMatcher(known);
        this.decorations = this.matcher.createDeco(update.view);
      } else if (update.docChanged || update.viewportChanged) {
        this.decorations = this.matcher.updateDeco(update, this.decorations);
      }
    }
  },
  { decorations: (v) => v.decorations },
);

export interface StackSpecEditorProps {
  value: string;
  onChange?: (value: string) => void;
  editable: boolean;
  known: KnownNames;
  minHeight?: string;
}

export function StackSpecEditor({ value, onChange, editable, known, minHeight }: StackSpecEditorProps) {
  const hostRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;
  const navigate = useNavigate();
  const navigateRef = useRef(navigate);
  navigateRef.current = navigate;
  const knownRef = useRef(known);
  knownRef.current = known;
  const editableCompartment = useRef(new Compartment());
  const knownCompartment = useRef(new Compartment());

  useEffect(() => {
    if (!hostRef.current) return;

    const clickHandler = EditorView.domEventHandlers({
      mousedown(event, view) {
        if (!(event.ctrlKey || event.metaKey)) return false;
        const target = event.target as HTMLElement;
        if (!target.closest('.cm-object-link')) return false;
        const pos = view.posAtDOM(target);
        const line = view.state.doc.lineAt(pos);
        const col = pos - line.from;
        const re = /[A-Za-z0-9][A-Za-z0-9._-]*/g;
        let m: RegExpExecArray | null;
        while ((m = re.exec(line.text))) {
          if (col >= m.index && col <= m.index + m[0].length) {
            const kind = buildObjectIndex(knownRef.current).get(m[0]);
            if (kind) {
              event.preventDefault();
              navigateRef.current(objectRouteFor(kind, m[0]));
            }
            return true;
          }
        }
        return false;
      },
    });

    const state = EditorState.create({
      doc: value,
      extensions: [
        history(),
        keymap.of([...defaultKeymap, ...historyKeymap, ...completionKeymap]),
        yaml(),
        syntaxHighlighting(stackHighlightStyle, { fallback: true }),
        closeBrackets(),
        autocompletion({ override: [stackSpecCompletionSource(() => knownRef.current)] }),
        knownCompartment.current.of(knownNamesFacet.of(known)),
        objectLinkPlugin,
        clickHandler,
        theme,
        EditorView.lineWrapping,
        editableCompartment.current.of([EditorView.editable.of(editable), EditorState.readOnly.of(!editable)]),
        EditorView.updateListener.of((update) => {
          if (update.docChanged) onChangeRef.current?.(update.state.doc.toString());
        }),
        ...(minHeight ? [EditorView.theme({ '.cm-scroller': { minHeight } })] : []),
      ],
    });

    const view = new EditorView({ state, parent: hostRef.current });
    viewRef.current = view;
    return () => {
      view.destroy();
      viewRef.current = null;
    };
    // Only re-created on mount; editable/value/known changes are pushed via
    // the effects below instead of tearing the editor down (which would lose
    // cursor position and undo history on every keystroke elsewhere on the
    // page).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    const current = view.state.doc.toString();
    if (current !== value) {
      view.dispatch({ changes: { from: 0, to: current.length, insert: value } });
    }
  }, [value]);

  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    view.dispatch({
      effects: editableCompartment.current.reconfigure([
        EditorView.editable.of(editable),
        EditorState.readOnly.of(!editable),
      ]),
    });
  }, [editable]);

  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    view.dispatch({ effects: knownCompartment.current.reconfigure(knownNamesFacet.of(known)) });
  }, [known]);

  return <div ref={hostRef} className="stack-spec-editor" />;
}
