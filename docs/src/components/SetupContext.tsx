import React, {createContext, useCallback, useContext, useEffect, useMemo, useState} from 'react';

const PREFIX = 'clouduler-docs.setup.';

interface SetupCtx {
  values: Record<string, string>;
  defaults: Record<string, string>;
  set: (name: string, value: string) => void;
  registerDefaults: (d: Record<string, string>) => void;
}

const Ctx = createContext<SetupCtx | null>(null);

function readStored(): Record<string, string> {
  const out: Record<string, string> = {};
  try {
    for (let i = 0; i < localStorage.length; i++) {
      const k = localStorage.key(i);
      if (k?.startsWith(PREFIX)) out[k.slice(PREFIX.length)] = localStorage.getItem(k) ?? '';
    }
  } catch {
    // storage blocked
  }
  return out;
}

export function SetupProvider({children}: {children?: React.ReactNode}): React.JSX.Element {
  const [stored, setStored] = useState<Record<string, string>>({});
  const [defaults, setDefaults] = useState<Record<string, string>>({});

  useEffect(() => {
    setStored(readStored());
    const onStorage = (e: StorageEvent) => {
      if (e.key?.startsWith(PREFIX)) setStored(readStored());
    };
    window.addEventListener('storage', onStorage);
    return () => window.removeEventListener('storage', onStorage);
  }, []);

  const set = useCallback((name: string, value: string) => {
    setStored((s) => (s[name] === value ? s : {...s, [name]: value}));
    try {
      localStorage.setItem(PREFIX + name, value);
    } catch {
      // ignore
    }
  }, []);

  const registerDefaults = useCallback((d: Record<string, string>) => {
    setDefaults((cur) => {
      const changed = Object.keys(d).some((k) => cur[k] !== d[k]);
      return changed ? {...cur, ...d} : cur;
    });
  }, []);

  const values = useMemo(() => {
    const v = {...defaults};
    for (const [k, val] of Object.entries(stored)) if (val !== '') v[k] = val;
    return v;
  }, [defaults, stored]);

  const ctx = useMemo(() => ({values, defaults, set, registerDefaults}), [values, defaults, set, registerDefaults]);
  return <Ctx.Provider value={ctx}>{children}</Ctx.Provider>;
}

const FALLBACK: SetupCtx = {values: {}, defaults: {}, set: () => {}, registerDefaults: () => {}};
export function useSetup(): SetupCtx {
  return useContext(Ctx) ?? FALLBACK;
}
