import { createContext, useCallback, useContext, useMemo, useRef, useState, type ReactNode } from 'react';
import { Toast, type NoticeTone } from './overlays';

interface ToastItem {
  id: number;
  tone: NoticeTone;
  title: string;
  body?: string;
}

interface ToastApi {
  /** Shows a toast for a few seconds; errors stay longer. */
  toast: (title: string, opts?: { tone?: NoticeTone; body?: string }) => void;
}

const ToastContext = createContext<ToastApi | null>(null);

export function ToastProvider({ children }: { children: ReactNode }) {
  const [items, setItems] = useState<ToastItem[]>([]);
  const next = useRef(1);

  const dismiss = useCallback((id: number) => setItems((cur) => cur.filter((t) => t.id !== id)), []);
  const toast = useCallback<ToastApi['toast']>(
    (title, opts) => {
      const id = next.current++;
      const tone = opts?.tone ?? 'ok';
      setItems((cur) => [...cur.slice(-3), { id, tone, title, body: opts?.body }]);
      window.setTimeout(() => dismiss(id), tone === 'danger' ? 8000 : 4000);
    },
    [dismiss],
  );
  const api = useMemo(() => ({ toast }), [toast]);

  return (
    <ToastContext.Provider value={api}>
      {children}
      <div className="cl-toasts" aria-live="polite">
        {items.map((t) => (
          <Toast key={t.id} tone={t.tone} title={t.title} onClose={() => dismiss(t.id)}>
            {t.body}
          </Toast>
        ))}
      </div>
    </ToastContext.Provider>
  );
}

/** Outside a provider (a unit test of one view) toasts are dropped, not thrown. */
export function useToast(): ToastApi['toast'] {
  return useContext(ToastContext)?.toast ?? (() => {});
}
