// Typed input wrappers used by the config form, re-skinned onto the design
// system's TextField, Toggle and cl-input styles. Each validates its own value
// and reports up through onChange so the parent doesn't need to know the
// validation rule for every field kind.

import { useId, type ReactNode } from 'react';
import { HelpTip, Icon } from '../ds';
import { isValidCidr, isValidDuration, isValidIp, isValidPort } from '../lib/validate';

interface FieldProps<T> {
  id: string;
  /** Short human name, sentence case. */
  label: string;
  value: T;
  onChange: (value: T) => void;
  /** One short line under the input, or nothing. */
  hint?: ReactNode;
  /** The longer explanation, shown behind a "?" next to the label. */
  help?: ReactNode;
  /** The raw config key, shown small inside the "?" text. */
  configKey?: string;
  disabled?: boolean;
}

/** Label row (name plus "?"), the control, then the one-line hint or error. */
function Shell({
  label,
  help,
  configKey,
  hint,
  error,
  htmlFor,
  hintId,
  children,
}: {
  label: string;
  help?: ReactNode;
  configKey?: string;
  hint?: ReactNode;
  error?: string;
  htmlFor: string;
  hintId: string;
  children: ReactNode;
}) {
  return (
    <div className="cl-field fld">
      <div className="fld__head">
        <label className="cl-label fld__label" htmlFor={htmlFor}>
          {label}
        </label>
        {(help || configKey) && <HelpTip label={label} configKey={configKey}>{help}</HelpTip>}
      </div>
      {children}
      {(error || hint) && (
        <div id={hintId} className={error ? 'cl-hint cl-hint--error' : 'cl-hint fld__hint'}>
          {error ? (
            <>
              <Icon name="x" size={14} />
              <span>{error}</span>
            </>
          ) : (
            hint
          )}
        </div>
      )}
    </div>
  );
}

function Input({
  label,
  value,
  onChange,
  hint,
  help,
  configKey,
  disabled,
  error,
  placeholder,
  type = 'text',
  mono,
  numeric,
}: Omit<FieldProps<string>, 'id'> & { error?: string; placeholder?: string; type?: string; mono?: boolean; numeric?: boolean }) {
  const id = useId();
  const hid = `${id}-h`;
  return (
    <Shell label={label} help={help} configKey={configKey} hint={hint} error={error} htmlFor={id} hintId={hid}>
      <input
        id={id}
        type={type}
        inputMode={numeric ? 'numeric' : undefined}
        className={`cl-input${mono ? ' cl-input--mono' : ''}${error ? ' cl-input--error' : ''}`}
        value={value}
        placeholder={placeholder}
        onChange={(e) => onChange(e.target.value)}
        aria-invalid={error ? true : undefined}
        aria-describedby={error || hint ? hid : undefined}
        autoComplete={type === 'password' ? 'new-password' : 'off'}
        disabled={disabled}
      />
    </Shell>
  );
}

export function TextField({ id: _id, ...p }: FieldProps<string>) {
  return <Input {...p} />;
}

export function NumberField({ id: _id, value, onChange, ...p }: FieldProps<number>) {
  return <Input {...p} type="number" numeric mono value={String(value)} onChange={(v) => onChange(Number(v))} />;
}

export function CheckboxField({ label, value, onChange, hint, help, configKey, disabled }: FieldProps<boolean>) {
  return (
    <div className="cl-toggle fld-toggle">
      <div className="cl-toggle__text">
        <span className="cl-toggle__label fld__head">
          {label}
          {(help || configKey) && <HelpTip label={label} configKey={configKey}>{help}</HelpTip>}
        </span>
        {hint && <span className="cl-sm cl-muted">{hint}</span>}
      </div>
      <button
        type="button"
        role="switch"
        aria-checked={value}
        aria-label={label}
        disabled={disabled}
        className={`cl-switch${value ? ' cl-switch--on' : ''}`}
        onClick={() => onChange(!value)}
      >
        <span className="cl-switch__knob">{value && <Icon name="check" size={14} />}</span>
      </button>
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
  help,
  configKey,
  disabled,
}: FieldProps<T> & { options: { value: T; label: string }[] }) {
  return (
    <Shell label={label} help={help} configKey={configKey} hint={hint} htmlFor={id} hintId={`${id}-h`}>
      <select id={id} className="cl-input" value={value} onChange={(e) => onChange(e.target.value as T)} disabled={disabled}>
        {options.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      </select>
    </Shell>
  );
}

function Checked({
  valid,
  error,
  placeholder,
  id: _id,
  ...p
}: FieldProps<string> & { valid: boolean; error: string; placeholder?: string }) {
  return <Input {...p} mono placeholder={placeholder} error={valid ? undefined : error} />;
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

export function TimeOfDayField({ id: _id, ...p }: FieldProps<string>) {
  return <Input {...p} type="time" mono />;
}

export function PortField({ id: _id, value, onChange, ...p }: FieldProps<number>) {
  return (
    <Input
      {...p}
      type="number"
      numeric
      mono
      value={String(value)}
      onChange={(v) => onChange(Number(v))}
      error={isValidPort(value) ? undefined : 'Port must be between 1 and 65535.'}
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
