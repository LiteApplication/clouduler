import { createContext, useCallback, useContext, useMemo, useRef, useState, type ReactNode } from 'react';
import { ConfirmDialog } from './overlays';

export interface ConfirmOptions {
  title: string;
  body?: ReactNode;
  confirmLabel?: string;
  cancelLabel?: string;
  confirmVariant?: 'danger' | 'primary' | 'secondary';
  /** The user must type this exactly before the confirm button enables. */
  requireText?: string;
}

type ConfirmFn = (opts: ConfirmOptions) => Promise<boolean>;

const ConfirmContext = createContext<ConfirmFn | null>(null);

const nativeConfirm: ConfirmFn = async ({ title, requireText }) => {
  if (requireText) return window.prompt(`${title}\nType "${requireText}" to confirm.`) === requireText;
  return window.confirm(title);
};

export function ConfirmProvider({ children }: { children: ReactNode }) {
  const [opts, setOpts] = useState<ConfirmOptions | null>(null);
  const resolver = useRef<((ok: boolean) => void) | null>(null);

  const confirm = useCallback<ConfirmFn>((o) => {
    // A second confirm while one is open cancels the first rather than leaking it.
    resolver.current?.(false);
    return new Promise<boolean>((resolve) => {
      resolver.current = resolve;
      setOpts(o);
    });
  }, []);

  const settle = (ok: boolean) => {
    resolver.current?.(ok);
    resolver.current = null;
    setOpts(null);
  };
  const api = useMemo(() => confirm, [confirm]);

  return (
    <ConfirmContext.Provider value={api}>
      {children}
      <ConfirmDialog
        open={opts !== null}
        title={opts?.title ?? ''}
        confirmLabel={opts?.confirmLabel}
        cancelLabel={opts?.cancelLabel}
        confirmVariant={opts?.confirmVariant}
        requireText={opts?.requireText}
        onConfirm={() => settle(true)}
        onCancel={() => settle(false)}
      >
        {opts?.body}
      </ConfirmDialog>
    </ConfirmContext.Provider>
  );
}

/** A promise API over ConfirmDialog: `if (await confirm({ title, requireText })) ...`.
 *  Replaces window.confirm. Resolves false on cancel, Esc or scrim click. */
export function useConfirm(): ConfirmFn {
  // Outside a provider (isolated tests, stray roots) fall back to the
  // browser prompt rather than crashing the view.
  return useContext(ConfirmContext) ?? nativeConfirm;
}
