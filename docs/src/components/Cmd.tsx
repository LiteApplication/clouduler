import React from 'react';
import CodeBlock from '@theme/CodeBlock';
import {useSetup} from './SetupContext';
import styles from './Cmd.module.css';

const LINE_PREFIX = /^\[([a-z, ]+)\]\s?/;

/** Pure helper: substitute {{vars}} and filter `[distro,...] ` prefixed lines. */
export function renderCmd(
  text: string,
  values: Record<string, string>,
  distro: string | undefined,
): {text: string; missing: string[]} {
  const missing = new Set<string>();
  const lines: string[] = [];
  for (const raw of text.replace(/^\n+|\n+$/g, '').split('\n')) {
    const m = LINE_PREFIX.exec(raw);
    let line = raw;
    if (m && distro) {
      const allowed = m[1].split(',').map((s) => s.trim());
      if (!allowed.includes(distro)) continue;
      line = raw.slice(m[0].length);
    }
    lines.push(
      line.replace(/\{\{\s*([\w-]+)\s*\}\}/g, (all, name: string) => {
        const v = values[name];
        if (v === undefined || v === '') {
          missing.add(name);
          return all;
        }
        return v;
      }),
    );
  }
  return {text: lines.join('\n'), missing: [...missing]};
}

export default function Cmd({
  children,
  lang = 'bash',
  title,
}: {
  children: string;
  lang?: string;
  title?: string;
}): React.JSX.Element {
  const {values} = useSetup();
  const {text, missing} = renderCmd(String(children), values, values.distro);
  return (
    <div className={styles.wrap}>
      <CodeBlock language={lang} title={title}>
        {text}
      </CodeBlock>
      {missing.length > 0 && (
        <div className={styles.missing}>
          Not set yet: {missing.map((m) => <code key={m}>{`{{${m}}}`}</code>)} (fill in the setup form above)
        </div>
      )}
    </div>
  );
}
