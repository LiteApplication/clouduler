// The configuration draft, held above both the sidebar and the Configuration
// view. The sidebar submenu shows a per-page count of unsaved edits, and the
// pages themselves edit the same object, so the draft cannot live inside the
// Configuration view any more -- it would unmount the moment you navigated
// away and quietly drop the edits.

import { createContext, useCallback, useContext, useEffect, useState, type ReactNode } from 'react';
import { useLocation } from 'react-router-dom';
import { api, ApiError } from '../../api/client';
import type { ClusterConfig } from '../../api/types';
import { diffConfig, type DiffEntry } from '../../lib/configDiff';
import { normalizeConfig } from '../../lib/configNormalize';

function clone<T>(v: T): T {
  return JSON.parse(JSON.stringify(v));
}

interface ConfigDraftValue {
  original: ClusterConfig | null;
  draft: ClusterConfig | null;
  setDraft: (c: ClusterConfig) => void;
  /** Throws away local edits and goes back to what the cluster is running. */
  discard: () => void;
  /** Re-reads the running config, after an apply is confirmed or rolled back. */
  reload: () => void;
  diff: DiffEntry[];
  loadError: string | null;
}

const ConfigDraftContext = createContext<ConfigDraftValue | null>(null);

export function ConfigDraftProvider({ children }: { children: ReactNode }) {
  const [original, setOriginal] = useState<ClusterConfig | null>(null);
  const [draft, setDraft] = useState<ClusterConfig | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const onConfigPage = useLocation().pathname.startsWith('/configuration');

  const reload = useCallback(() => {
    api
      .getConfiguration()
      .then((raw) => {
        const cfg = normalizeConfig(raw);
        setOriginal(cfg);
        setDraft(clone(cfg));
        setLoadError(null);
      })
      .catch((e) => setLoadError(e instanceof ApiError ? e.message : String(e)));
  }, []);

  // Fetched the first time configuration is opened rather than on login: every
  // other page in the console runs off the event stream and has no use for it.
  useEffect(() => {
    if (onConfigPage && !original && !loadError) reload();
  }, [onConfigPage, original, loadError, reload]);

  const discard = useCallback(() => {
    if (original) setDraft(clone(original));
  }, [original]);

  const diff = original && draft ? diffConfig(original, draft) : [];

  return (
    <ConfigDraftContext.Provider value={{ original, draft, setDraft, discard, reload, diff, loadError }}>
      {children}
    </ConfigDraftContext.Provider>
  );
}

export function useConfigDraft(): ConfigDraftValue {
  const ctx = useContext(ConfigDraftContext);
  if (!ctx) throw new Error('useConfigDraft must be used inside ConfigDraftProvider');
  return ctx;
}

/** How many staged changes fall under the given top-level config keys. */
export function changesFor(diff: DiffEntry[], keys: string[]): number {
  return diff.filter((d) => keys.some((k) => d.path === k || d.path.startsWith(`${k}.`))).length;
}
