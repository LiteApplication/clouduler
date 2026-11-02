import React from 'react';
import {useSetup} from './SetupContext';

/**
 * Inline value from the SetupForm. Renders the current value (shared with every page),
 * or `fallback` (then `{{name}}`) highlighted when nothing is set.
 */
export default function Var({
  name,
  fallback,
  plain = false,
}: {
  name: string;
  fallback?: string;
  plain?: boolean;
}): React.JSX.Element {
  const {values} = useSetup();
  const v = values[name];
  const isSet = v !== undefined && v !== '';
  const text = isSet ? v : (fallback ?? `{{${name}}}`);
  const style: React.CSSProperties = isSet
    ? {}
    : {background: 'var(--ifm-color-warning-contrast-background, #fff4d6)', borderRadius: 3, padding: '0 0.25em'};
  return plain ? (
    <span style={style} title={`Setup value: ${name}`}>{text}</span>
  ) : (
    <code style={style} title={`Setup value: ${name}`}>{text}</code>
  );
}
