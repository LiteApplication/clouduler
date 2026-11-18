import { useEffect, useRef } from 'react';
import { closeBrackets, closeBracketsKeymap } from '@codemirror/autocomplete';
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import { bracketMatching, HighlightStyle, indentUnit, syntaxHighlighting } from '@codemirror/language';
import { highlightSelectionMatches, searchKeymap } from '@codemirror/search';
import { Compartment, EditorState, Prec } from '@codemirror/state';
import {
  drawSelection,
  EditorView,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  keymap,
  lineNumbers,
} from '@codemirror/view';
import { tags as t } from '@lezer/highlight';
import { specHighlightStyle, specTheme } from '../lib/editorTheme';
import { hcl } from '../lib/hcl';

// Colours for the tokens HCL has beyond the YAML spec editor's palette.
const hclHighlightStyle = HighlightStyle.define([
  { tag: t.keyword, color: 'var(--brand)', fontWeight: '600' },
  { tag: t.typeName, color: 'var(--warn)' },
  { tag: [t.function(t.variableName)], color: 'var(--pop)' },
  { tag: t.variableName, color: 'var(--ink)' },
  { tag: t.standard(t.variableName), color: 'var(--ink)', fontStyle: 'italic' },
  { tag: t.operator, color: 'var(--ink-muted)' },
  { tag: t.meta, color: 'var(--brand)' },
]);

const hclTheme = EditorView.theme({
  '.cm-scroller': { minHeight: '240px', maxHeight: '70vh', overflow: 'auto' },
  '.cm-lineNumbers .cm-gutterElement': { padding: '0 10px 0 14px', minWidth: '36px' },
  '.cm-content': { padding: '10px 0' },
  '.cm-line': { padding: '0 14px 0 8px' },
  '.cm-matchingBracket, &.cm-focused .cm-matchingBracket': {
    backgroundColor: 'var(--brand-bg)',
    outline: '1px solid var(--brand)',
    color: 'inherit',
  },
  '.cm-nonmatchingBracket': { color: 'var(--danger)' },
  '.cm-selectionMatch': { backgroundColor: 'color-mix(in srgb, var(--brand) 18%, transparent)' },
  '.cm-searchMatch': { backgroundColor: 'color-mix(in srgb, var(--warn) 30%, transparent)' },
  '.cm-searchMatch.cm-searchMatch-selected': { backgroundColor: 'color-mix(in srgb, var(--warn) 55%, transparent)' },
  '.cm-panels': { backgroundColor: 'var(--surface-1)', color: 'var(--ink)', borderColor: 'var(--line)' },
  '.cm-panels input, .cm-panels button': { fontFamily: 'inherit', fontSize: '12px' },
  '.cm-panels input[type=text]': {
    backgroundColor: 'var(--surface-2)',
    color: 'var(--ink)',
    border: '1px solid var(--line-strong)',
    borderRadius: 'var(--radius-sm, 4px)',
  },
  '.cm-panels button': {
    backgroundColor: 'var(--surface-2)',
    color: 'var(--ink)',
    border: '1px solid var(--line-strong)',
    borderRadius: 'var(--radius-sm, 4px)',
    backgroundImage: 'none',
  },
  '.cm-gutters': { borderRight: '1px solid var(--line)' },
});

export interface HclEditorProps {
  value: string;
  onChange?: (value: string) => void;
  editable: boolean;
  id?: string;
  ariaLabel: string;
  /** Called on Ctrl/Cmd+S. */
  onSave?: () => void;
}

/** CodeMirror editor for a Terraform/HCL module: highlighting, line numbers, search, bracket matching. */
export function HclEditor({ value, onChange, editable, id, ariaLabel, onSave }: HclEditorProps) {
  const hostRef = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;
  const onSaveRef = useRef(onSave);
  onSaveRef.current = onSave;
  const editableCompartment = useRef(new Compartment());

  useEffect(() => {
    if (!hostRef.current) return;
    const state = EditorState.create({
      doc: value,
      extensions: [
        lineNumbers(),
        highlightActiveLineGutter(),
        highlightSpecialChars(),
        history(),
        drawSelection(),
        bracketMatching(),
        closeBrackets(),
        highlightActiveLine(),
        highlightSelectionMatches(),
        indentUnit.of('  '),
        EditorState.tabSize.of(2),
        hcl(),
        syntaxHighlighting(specHighlightStyle),
        syntaxHighlighting(hclHighlightStyle),
        Prec.highest(
          keymap.of([
            {
              key: 'Mod-s',
              preventDefault: true,
              run: () => {
                onSaveRef.current?.();
                return true;
              },
            },
          ]),
        ),
        keymap.of([...closeBracketsKeymap, ...defaultKeymap, ...searchKeymap, ...historyKeymap, indentWithTab]),
        specTheme,
        hclTheme,
        EditorView.contentAttributes.of({ 'aria-label': ariaLabel, ...(id ? { id } : {}) }),
        editableCompartment.current.of([EditorView.editable.of(editable), EditorState.readOnly.of(!editable)]),
        EditorView.updateListener.of((update) => {
          if (update.docChanged) onChangeRef.current?.(update.state.doc.toString());
        }),
      ],
    });
    const view = new EditorView({ state, parent: hostRef.current });
    viewRef.current = view;
    return () => {
      view.destroy();
      viewRef.current = null;
    };
    // Created once; value/editable are pushed in below so cursor and undo history survive.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    const current = view.state.doc.toString();
    if (current !== value) view.dispatch({ changes: { from: 0, to: current.length, insert: value } });
  }, [value]);

  useEffect(() => {
    viewRef.current?.dispatch({
      effects: editableCompartment.current.reconfigure([EditorView.editable.of(editable), EditorState.readOnly.of(!editable)]),
    });
  }, [editable]);

  return <div ref={hostRef} className="stack-spec-editor hcl-editor" />;
}
