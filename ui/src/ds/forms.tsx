import { useId, useState, type ChangeEvent, type ReactNode } from 'react';
import { Icon } from './Icon';
import { cx } from './cx';

export interface TextFieldProps {
  label: string;
  value?: string;
  defaultValue?: string;
  placeholder?: string;
  type?: string;
  mono?: boolean;
  error?: string;
  hint?: ReactNode;
  disabled?: boolean;
  readOnly?: boolean;
  required?: boolean;
  autoFocus?: boolean;
  inputMode?: 'text' | 'numeric' | 'decimal' | 'email' | 'url' | 'search';
  autoComplete?: string;
  /** Renders a textarea instead of an input. */
  multiline?: boolean;
  rows?: number;
  name?: string;
  className?: string;
  onChange?: (e: ChangeEvent<HTMLInputElement & HTMLTextAreaElement>) => void;
}

export function TextField(p: TextFieldProps) {
  const id = useId();
  const hid = `${id}-h`;
  const common = {
    id,
    name: p.name,
    className: cx('cl-input', p.mono && 'cl-input--mono', p.error && 'cl-input--error'),
    value: p.value,
    defaultValue: p.defaultValue,
    placeholder: p.placeholder,
    onChange: p.onChange,
    disabled: p.disabled,
    readOnly: p.readOnly,
    required: p.required,
    autoFocus: p.autoFocus,
    'aria-invalid': p.error ? true : undefined,
    'aria-describedby': p.error || p.hint ? hid : undefined,
    // Browsers ignore "off" on a password field and offer saved logins; only
    // "new-password" stops that. The data attributes do the same for the
    // 1Password and LastPass extensions.
    autoComplete: p.autoComplete ?? (p.type === 'password' ? 'new-password' : 'off'),
    ...(p.type === 'password' ? { 'data-1p-ignore': true, 'data-lpignore': 'true' } : {}),
  };
  return (
    <div className={cx('cl-field', p.className)}>
      <label className="cl-label" htmlFor={id}>
        {p.label}
      </label>
      {p.multiline ? (
        <textarea {...common} rows={p.rows} />
      ) : (
        <input {...common} type={p.type ?? 'text'} inputMode={p.inputMode} />
      )}
      {(p.error || p.hint) && (
        <div id={hid} className={cx('cl-hint', p.error && 'cl-hint--error')}>
          {p.error ? (
            <>
              <Icon name="x" size={14} />
              <span>{p.error}</span>
            </>
          ) : (
            p.hint
          )}
        </div>
      )}
    </div>
  );
}

export function Toggle({
  label,
  description,
  checked,
  defaultChecked,
  disabled,
  onChange,
  className,
}: {
  label: string;
  description?: ReactNode;
  /** Controlled when given; otherwise the toggle keeps its own state. */
  checked?: boolean;
  defaultChecked?: boolean;
  disabled?: boolean;
  onChange?: (on: boolean) => void;
  className?: string;
}) {
  const [own, setOwn] = useState(!!defaultChecked);
  const on = checked ?? own;
  const flip = () => {
    if (disabled) return;
    if (checked == null) setOwn(!on);
    onChange?.(!on);
  };
  return (
    <div className={cx('cl-toggle', className)}>
      <div className="cl-toggle__text">
        <span className="cl-toggle__label">{label}</span>
        {description && <span className="cl-sm cl-muted">{description}</span>}
      </div>
      <button
        type="button"
        role="switch"
        aria-checked={on}
        aria-label={label}
        disabled={disabled}
        className={cx('cl-switch', on && 'cl-switch--on')}
        onClick={flip}
      >
        <span className="cl-switch__knob">{on && <Icon name="check" size={14} />}</span>
      </button>
    </div>
  );
}
