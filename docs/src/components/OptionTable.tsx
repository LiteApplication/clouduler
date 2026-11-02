import React, {useState} from 'react';
import styles from './OptionTable.module.css';

export interface OptionRow {
  key: string;
  type: string;
  default?: string;
  reload: 'live' | 'restart' | 'n/a';
  required?: boolean;
  description: React.ReactNode;
  quirks?: React.ReactNode;
}

export function optionAnchor(key: string): string {
  return 'opt-' + key.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '');
}

const RELOAD_LABEL = {live: 'live reload', restart: 'needs restart', 'n/a': 'n/a'} as const;

export default function OptionTable({rows}: {rows: OptionRow[]}): React.JSX.Element {
  const [q, setQ] = useState('');
  const needle = q.trim().toLowerCase();
  const shown = needle
    ? rows.filter((r) =>
        [r.key, r.type, r.default ?? '', typeof r.description === 'string' ? r.description : '']
          .join(' ')
          .toLowerCase()
          .includes(needle),
      )
    : rows;
  return (
    <div className={styles.wrap}>
      {rows.length > 8 && (
        <input
          className={styles.filter}
          type="search"
          placeholder={`Filter ${rows.length} options...`}
          aria-label="Filter options"
          value={q}
          onChange={(e) => setQ(e.target.value)}
        />
      )}
      <div className={styles.list} role="table">
        <div className={`${styles.row} ${styles.head}`} role="row">
          <div role="columnheader">Option</div>
          <div role="columnheader">Type / default</div>
          <div role="columnheader">Reload</div>
          <div role="columnheader">Description</div>
        </div>
        {shown.map((r) => {
          const id = optionAnchor(r.key);
          return (
            <div className={styles.row} role="row" key={r.key} id={id}>
              <div className={styles.key} role="cell">
                <a href={`#${id}`} className={styles.anchor} aria-label={`Link to ${r.key}`}>
                  <code>{r.key}</code>
                </a>
                {r.required && <span className={styles.req}>required</span>}
              </div>
              <div role="cell" className={styles.type}>
                <code>{r.type}</code>
                <span className={styles.def}>
                  {r.default !== undefined ? <>default <code>{r.default}</code></> : 'no default'}
                </span>
              </div>
              <div role="cell">
                <span className={`${styles.badge} ${styles[r.reload === 'n/a' ? 'na' : r.reload]}`}>{RELOAD_LABEL[r.reload]}</span>
              </div>
              <div role="cell" className={styles.desc}>
                <div>{r.description}</div>
                {r.quirks && <div className={styles.quirks}><strong>Watch out:</strong> {r.quirks}</div>}
              </div>
            </div>
          );
        })}
        {shown.length === 0 && <div className={styles.empty}>No option matches "{q}".</div>}
      </div>
    </div>
  );
}
