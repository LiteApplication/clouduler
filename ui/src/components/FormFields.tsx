// Typed input wrappers used by the config form, re-skinned onto the design
// system's TextField, Toggle and cl-input styles. Each validates its own value
// and reports up through onChange so the parent doesn't need to know the
// validation rule for every field kind.

import { Toggle, TextField as DsTextField } from '../ds';
import { isValidCidr, isValidDuration, isValidIp, isValidPort } from '../lib/validate';

interface FieldProps<T> {
  id: string;
  label: string;
  value: T;
  onChange: (value: T) => void;
  hint?: string;
  disabled?: boolean;
}

export function TextField({ label, value, onChange, hint, disabled }: FieldProps<string>) {
  return <DsTextField label={label} value={value} onChange={(e) => onChange(e.target.value)} hint={hint} disabled={disabled} />;
}

export function NumberField({ label, value, onChange, hint, disabled }: FieldProps<number>) {
  return (
    <DsTextField
      label={label}
      type="number"
      inputMode="numeric"
      mono
      value={String(value)}
      onChange={(e) => onChange(Number(e.target.value))}
      hint={hint}
      disabled={disabled}
    />
  );
}

export function CheckboxField({ label, value, onChange, hint, disabled }: FieldProps<boolean>) {
  return <Toggle label={label} description={hint} checked={value} onChange={onChange} disabled={disabled} />;
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
    <div className="cl-field">
      <label className="cl-label" htmlFor={id}>
        {label}
      </label>
      <select id={id} className="cl-input" value={value} onChange={(e) => onChange(e.target.value as T)} disabled={disabled}>
        {options.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      </select>
      {hint && <div className="cl-hint">{hint}</div>}
    </div>
  );
}

function Checked({
  label,
  value,
  onChange,
  hint,
  disabled,
  valid,
  error,
  placeholder,
}: Omit<FieldProps<string>, 'id'> & { valid: boolean; error: string; placeholder?: string }) {
  return (
    <DsTextField
      label={label}
      mono
      value={value}
      placeholder={placeholder}
      onChange={(e) => onChange(e.target.value)}
      error={valid ? undefined : error}
      hint={hint}
      disabled={disabled}
    />
  );
}

export function IpField(p: FieldProps<string>) {
  return <Checked {...p} valid={p.value.length === 0 || isValidIp(p.value)} error="Not a valid IP address." />;
}

export function CidrField(p: FieldProps<string>) {
  return (
    <Checked {...p} placeholder="10.10.0.0/24" valid={p.value.length === 0 || isValidCidr(p.value)} error="Not a valid CIDR subnet." />
  );
}

export function DurationField(p: FieldProps<string>) {
  return (
    <Checked
      {...p}
      placeholder="30s"
      valid={p.value.length === 0 || isValidDuration(p.value)}
      error="Not a valid duration. Try 30s, 5m or 1h30m."
    />
  );
}

export function TimeOfDayField({ label, value, onChange, hint, disabled }: FieldProps<string>) {
  return (
    <DsTextField label={label} type="time" mono value={value} onChange={(e) => onChange(e.target.value)} hint={hint} disabled={disabled} />
  );
}

export function PortField({ label, value, onChange, hint, disabled }: FieldProps<number>) {
  return (
    <DsTextField
      label={label}
      type="number"
      inputMode="numeric"
      mono
      value={String(value)}
      onChange={(e) => onChange(Number(e.target.value))}
      error={isValidPort(value) ? undefined : 'Port must be between 1 and 65535.'}
      hint={hint}
      disabled={disabled}
    />
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
