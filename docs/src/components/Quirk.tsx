import React from 'react';
import styles from './Quirk.module.css';

const LABEL = {info: 'Good to know', warn: 'Quirk', danger: 'Trap'} as const;
const ICON = {info: 'i', warn: '!', danger: '×'} as const;

export default function Quirk({
  title,
  severity = 'warn',
  children,
}: {
  title: string;
  severity?: 'info' | 'warn' | 'danger';
  children?: React.ReactNode;
}): React.JSX.Element {
  return (
    <aside className={`${styles.quirk} ${styles[severity]}`}>
      <div className={styles.badge} aria-hidden="true">{ICON[severity]}</div>
      <div className={styles.body}>
        <div className={styles.head}>
          <span className={styles.kind}>{LABEL[severity]}</span>
          <strong className={styles.title}>{title}</strong>
        </div>
        <div className={styles.content}>{children}</div>
      </div>
    </aside>
  );
}
