import React, {useEffect} from 'react';
import {useSetup} from './SetupContext';
import styles from './SetupForm.module.css';

export interface SetupField {
  name: string;
  label: string;
  default?: string;
  help?: string;
  type?: 'text' | 'select';
  options?: Array<string | {value: string; label: string}>;
}

export const DISTRO_FIELD: SetupField = {
  name: 'distro',
  label: 'Distribution',
  type: 'select',
  default: 'debian',
  options: [
    {value: 'arch', label: 'Arch Linux'},
    {value: 'debian', label: 'Debian'},
    {value: 'raspbian', label: 'Raspberry Pi OS'},
  ],
};

export default function SetupForm({fields, title = 'Your setup'}: {fields: SetupField[]; title?: string}): React.JSX.Element {
  const {values, set, registerDefaults} = useSetup();

  useEffect(() => {
    const d: Record<string, string> = {};
    for (const f of fields) {
      if (f.default !== undefined) d[f.name] = f.default;
      else if (f.type === 'select' && f.options?.length) {
        const o = f.options[0];
        d[f.name] = typeof o === 'string' ? o : o.value;
      }
    }
    registerDefaults(d);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [JSON.stringify(fields)]);

  return (
    <form className={styles.form} onSubmit={(e) => e.preventDefault()}>
      <div className={styles.title}>{title}</div>
      <div className={styles.grid}>
        {fields.map((f) => {
          const id = `setup-${f.name}`;
          const val = values[f.name] ?? f.default ?? '';
          return (
            <label key={f.name} htmlFor={id} className={styles.field}>
              <span className={styles.label}>
                {f.label} <code>{`{{${f.name}}}`}</code>
              </span>
              {f.type === 'select' ? (
                <select id={id} value={val} onChange={(e) => set(f.name, e.target.value)}>
                  {(f.options ?? []).map((o) => {
                    const v = typeof o === 'string' ? o : o.value;
                    const l = typeof o === 'string' ? o : o.label;
                    return <option key={v} value={v}>{l}</option>;
                  })}
                </select>
              ) : (
                <input id={id} type="text" value={val} spellCheck={false} autoComplete="off" onChange={(e) => set(f.name, e.target.value)} />
              )}
              {f.help && <span className={styles.help}>{f.help}</span>}
            </label>
          );
        })}
      </div>
    </form>
  );
}
