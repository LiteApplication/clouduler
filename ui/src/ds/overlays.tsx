import { useEffect, useId, useRef, useState, type ReactNode } from 'react';
import { Button } from './Button';
import { TextField } from './forms';
import { Icon, type IconName } from './Icon';
import { cx } from './cx';

const FOCUSABLE = 'a[href],button:not([disabled]),input:not([disabled]),textarea:not([disabled]),select:not([disabled]),[tabindex]:not([tabindex="-1"])';

export function BottomSheet({
  open,
  title,
  onClose,
  children,
  footer,
  alert,
  contained,
  className,
}: {
  open: boolean;
  title: string;
  onClose?: () => void;
  children?: ReactNode;
  footer?: ReactNode;
  /** alertdialog semantics, for confirmations. */
  alert?: boolean;
  /** Positions inside the nearest positioned ancestor, for previews. */
  contained?: boolean;
  className?: string;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const id = useId();
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  // Focus moves into the sheet on open, is trapped while it is open, and
  // returns to whatever opened it on close; Esc closes.
  useEffect(() => {
    if (!open) return;
    const prev = document.activeElement as HTMLElement | null;
    const el = ref.current;
    ((el?.querySelector(FOCUSABLE) as HTMLElement | null) ?? el)?.focus();
    const key = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onCloseRef.current?.();
      if (e.key === 'Tab' && el) {
        const fs = el.querySelectorAll<HTMLElement>(FOCUSABLE);
        if (!fs.length) return;
        const first = fs[0];
        const last = fs[fs.length - 1];
        if (e.shiftKey && document.activeElement === first) {
          e.preventDefault();
          last.focus();
        } else if (!e.shiftKey && document.activeElement === last) {
          e.preventDefault();
          first.focus();
        }
      }
    };
    document.addEventListener('keydown', key);
    return () => {
      document.removeEventListener('keydown', key);
      prev?.focus?.();
    };
  }, [open]);

  if (!open) return null;
  return (
    <div className={cx('cl-sheetwrap', contained && 'cl-sheetwrap--contained')}>
      <div className="cl-scrim" onClick={onClose} />
      <div
        ref={ref}
        className={cx('cl-sheet', className)}
        role={alert ? 'alertdialog' : 'dialog'}
        aria-modal="true"
        aria-labelledby={id}
        tabIndex={-1}
      >
        <div className="cl-sheet__grab" aria-hidden="true" />
        <div className="cl-sheet__head">
          <h2 id={id} className="cl-sheet__title">
            {title}
          </h2>
          <Button iconOnly icon="close" label="Close" variant="ghost" onClick={onClose} />
        </div>
        <div className="cl-sheet__body">{children}</div>
        {footer && <div className="cl-sheet__foot">{footer}</div>}
      </div>
    </div>
  );
}

export function ConfirmDialog({
  open,
  title,
  children,
  confirmLabel = 'Confirm',
  cancelLabel = 'Cancel',
  confirmVariant = 'danger',
  requireText,
  busy,
  contained,
  onConfirm,
  onCancel,
}: {
  open: boolean;
  title: string;
  children?: ReactNode;
  confirmLabel?: string;
  cancelLabel?: string;
  confirmVariant?: 'danger' | 'primary' | 'secondary';
  /** The confirm button stays disabled until this exact text is typed. */
  requireText?: string;
  busy?: boolean;
  contained?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const [txt, setTxt] = useState('');
  useEffect(() => setTxt(''), [open]);
  const ok = !requireText || txt === requireText;
  return (
    <BottomSheet
      open={open}
      title={title}
      onClose={onCancel}
      alert
      contained={contained}
      footer={
        <>
          <Button variant="secondary" onClick={onCancel}>
            {cancelLabel}
          </Button>
          <Button variant={confirmVariant} disabled={!ok} loading={busy} onClick={onConfirm}>
            {confirmLabel}
          </Button>
        </>
      }
    >
      <div className="cl-confirm">
        {children && <div className="cl-confirm__body">{children}</div>}
        {requireText && (
          <TextField
            label={`Type "${requireText}" to confirm`}
            mono
            value={txt}
            onChange={(e) => setTxt(e.target.value)}
            autoFocus
          />
        )}
      </div>
    </BottomSheet>
  );
}

export type NoticeTone = 'ok' | 'warn' | 'danger' | 'info';
const TONE_ICON: Record<NoticeTone, IconName> = { ok: 'check-circle', warn: 'alert', danger: 'x', info: 'info' };

interface NoticeProps {
  tone?: NoticeTone;
  title?: string;
  children?: ReactNode;
  action?: string;
  onAction?: () => void;
  onClose?: () => void;
  className?: string;
}

function Notice({ tone = 'info', title, children, action, onAction, onClose, className, kind }: NoticeProps & { kind: string }) {
  return (
    <div className={cx('cl-notice', `cl-notice--${tone}`, kind, className)} role={tone === 'danger' ? 'alert' : 'status'}>
      <Icon name={TONE_ICON[tone]} size={20} />
      <div className="cl-notice__text">
        {title && <strong>{title}</strong>}
        {children && <span>{children}</span>}
      </div>
      {action && (
        <Button size="sm" variant="ghost" onClick={onAction}>
          {action}
        </Button>
      )}
      {onClose && <Button iconOnly size="sm" icon="close" label="Dismiss" variant="ghost" onClick={onClose} />}
    </div>
  );
}

export function Toast(p: NoticeProps) {
  return <Notice {...p} kind="cl-toast" />;
}
export function Banner(p: NoticeProps) {
  return <Notice {...p} kind="cl-banner" />;
}

export function EmptyState({
  icon = 'stack',
  title,
  children,
  action,
  className,
}: {
  icon?: IconName;
  title: string;
  children?: ReactNode;
  action?: ReactNode;
  className?: string;
}) {
  return (
    <div className={cx('cl-empty', className)}>
      <div className="cl-empty__art" aria-hidden="true">
        <Icon name={icon} size={40} />
      </div>
      <h3 className="cl-empty__title">{title}</h3>
      {children && <p className="cl-empty__body">{children}</p>}
      {action && <div>{action}</div>}
    </div>
  );
}
