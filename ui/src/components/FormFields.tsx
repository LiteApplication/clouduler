// Small typed input wrappers used by the config form. Each validates its own
// value and reports up through onChange/onValid so the parent doesn't need
// to know the validation rule for every field kind.

import { isValidCidr, isValidDuration, isValidIp, isValidPort } from '../lib/validate';

interface FieldProps<T> {
  id: string;
  label: string;
  value: T;
  onChange: (value: T) => void;
  hint?: string;
  disabled?: boolean;
}

export function TextField({ id, label, value, onChange, hint, disabled }: FieldProps<string>) {
  return (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      <input id={id} type="text" value={value} onChange={(e) => onChange(e.target.value)} disabled={disabled} />
      {hint && <span className="field-hint">{hint}</span>}
    </div>
  );
}

export function NumberField({
  id,
  label,
  value,
  onChange,
  hint,
  disabled,
}: FieldProps<number>) {
  return (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      <input
        id={id}
        type="number"
        value={value}
        onChange={(e) => onChange(Number(e.target.value))}
        disabled={disabled}
      />
      {hint && <span className="field-hint">{hint}</span>}
    </div>
  );
}

export function CheckboxField({ id, label, value, onChange, hint, disabled }: FieldProps<boolean>) {
  return (
    <div className="field">
      <label className="flex-row" htmlFor={id}>
        <input id={id} type="checkbox" checked={value} onChange={(e) => onChange(e.target.checked)} disabled={disabled} />
        {label}
      </label>
      {hint && <span className="field-hint">{hint}</span>}
    </div>
  );
}

export function SelectField<T extends string>({
  id,
  label,
  value,
  onChange,
  options,
  hint,
  disabled,
}: FieldProps<T> & { options: { value: T; label: string }[] }) {
  return (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      <select id={id} value={value} onChange={(e) => onChange(e.target.value as T)} disabled={disabled}>
        {options.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      </select>
      {hint && <span className="field-hint">{hint}</span>}
    </div>
  );
}

export function IpField({ id, label, value, onChange, hint, disabled }: FieldProps<string>) {
  const valid = value.length === 0 || isValidIp(value);
  return (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      <input
        id={id}
        type="text"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className={valid ? '' : 'invalid'}
        disabled={disabled}
      />
      {!valid && <span className="field-error">not a valid IP address</span>}
      {hint && valid && <span className="field-hint">{hint}</span>}
    </div>
  );
}

export function CidrField({ id, label, value, onChange, hint, disabled }: FieldProps<string>) {
  const valid = value.length === 0 || isValidCidr(value);
  return (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      <input
        id={id}
        type="text"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className={valid ? '' : 'invalid'}
        disabled={disabled}
        placeholder="10.10.0.0/24"
      />
      {!valid && <span className="field-error">not a valid CIDR subnet</span>}
      {hint && valid && <span className="field-hint">{hint}</span>}
    </div>
  );
}

export function DurationField({ id, label, value, onChange, hint, disabled }: FieldProps<string>) {
  const valid = value.length === 0 || isValidDuration(value);
  return (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      <input
        id={id}
        type="text"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className={valid ? '' : 'invalid'}
        disabled={disabled}
        placeholder="30s"
      />
      {!valid && <span className="field-error">not a valid duration (e.g. 30s, 5m, 1h30m)</span>}
      {hint && valid && <span className="field-hint">{hint}</span>}
    </div>
  );
}

export function TimeOfDayField({ id, label, value, onChange, hint, disabled }: FieldProps<string>) {
  return (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      <input id={id} type="time" value={value} onChange={(e) => onChange(e.target.value)} disabled={disabled} />
      {hint && <span className="field-hint">{hint}</span>}
    </div>
  );
}

export function PortField({ id, label, value, onChange, hint, disabled }: FieldProps<number>) {
  const valid = isValidPort(value);
  return (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      <input
        id={id}
        type="number"
        value={value}
        onChange={(e) => onChange(Number(e.target.value))}
        className={valid ? '' : 'invalid'}
        disabled={disabled}
      />
      {!valid && <span className="field-error">must be a port between 1 and 65535</span>}
      {hint && valid && <span className="field-hint">{hint}</span>}
    </div>
  );
}

/** Editable key/value list, for node drive maps and similar. When
 *  `keyOptions` is given, the key is picked from that list (e.g. the drives
 *  declared cluster-wide) instead of typed freehand, so it can't drift from
 *  the name the rest of the config actually uses. */
export function KeyValueList({
  entries,
  onChange,
  keyLabel,
  valueLabel,
  keyOptions,
  disabled,
}: {
  entries: [string, string][];
  onChange: (entries: [string, string][]) => void;
  keyLabel: string;
  valueLabel: string;
  keyOptions?: string[];
  disabled?: boolean;
}) {
  const update = (i: number, key: string, value: string) => {
    const next = [...entries];
    next[i] = [key, value];
    onChange(next);
  };
  const remove = (i: number) => onChange(entries.filter((_, idx) => idx !== i));
  const usedKeys = new Set(entries.map(([k]) => k));
  const nextOption = keyOptions?.find((k) => !usedKeys.has(k)) ?? '';
  const add = () => onChange([...entries, [nextOption, '']]);
  const canAdd = keyOptions === undefined || nextOption !== '';

  return (
    <div className="field">
      <div className="flex-row" style={{ marginBottom: 4 }}>
        <span className="muted" style={{ flex: 1 }}>
          {keyLabel}
        </span>
        <span className="muted" style={{ flex: 1 }}>
          {valueLabel}
        </span>
      </div>
      {entries.map(([k, v], i) => (
        <div className="flex-row" key={i} style={{ marginBottom: 4 }}>
          {keyOptions ? (
            <select
              value={k}
              onChange={(e) => update(i, e.target.value, v)}
              aria-label={`${keyLabel} ${i + 1}`}
              disabled={disabled}
              style={{ flex: 1 }}
            >
              {k && !keyOptions.includes(k) && <option value={k}>{k}</option>}
              {keyOptions
                .filter((opt) => opt === k || !usedKeys.has(opt))
                .map((opt) => (
                  <option key={opt} value={opt}>
                    {opt}
                  </option>
                ))}
            </select>
          ) : (
            <input
              type="text"
              value={k}
              onChange={(e) => update(i, e.target.value, v)}
              aria-label={`${keyLabel} ${i + 1}`}
              disabled={disabled}
              style={{ flex: 1 }}
            />
          )}
          <input
            type="text"
            value={v}
            onChange={(e) => update(i, k, e.target.value)}
            aria-label={`${valueLabel} ${i + 1}`}
            disabled={disabled}
            style={{ flex: 1 }}
          />
          {!disabled && (
            <button type="button" onClick={() => remove(i)} aria-label={`remove ${k || 'entry'}`}>
              Remove
            </button>
          )}
        </div>
      ))}
      {!disabled && keyOptions?.length === 0 && (
        <p className="muted">No drives are declared yet. Add one under Configuration &gt; Drives first.</p>
      )}
      {!disabled && canAdd && (
        <button type="button" onClick={add}>
          Add
        </button>
      )}
    </div>
  );
}
