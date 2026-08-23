// The configuration draft, held above both the sidebar and the Configuration
// view. The sidebar submenu shows a per-page count of unsaved edits, and the
// pages themselves edit the same object, so the draft cannot live inside the
// Configuration view any more, as it would unmount the moment you navigated
// away and quietly drop the edits.

import { createContext, useCallback, useContext, useEffect, useState, type ReactNode } from 'react';
import { useLocation } from 'react-router-dom';
import { api, ApiError } from '../../api/client';
import type { ClusterConfig, PendingConfig, ValidationError } from '../../api/types';
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
  // Node pages also need this: the list has to merge in nodes the config
  // declares but the live roster hasn't admitted yet (see `Nodes`), and node
  // detail pages edit a slice of this same config (see `NodeCard`).
  const pathname = useLocation().pathname;
  const onConfigPage = pathname.startsWith('/configuration') || pathname.startsWith('/nodes');

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

export interface ConfigApply {
  errors: ValidationError[];
  pending: PendingConfig | null;
  busy: boolean;
  validate: (config: ClusterConfig) => Promise<boolean>;
  apply: (config: ClusterConfig, rollbackAfter: string) => Promise<boolean>;
  confirm: () => Promise<void>;
  rollback: () => Promise<void>;
  clearErrors: () => void;
}

/**
 * Validate/apply/confirm/rollback against `/api/config`, plus the pending
 * rollback timer poll, shared by `Configuration` (the whole draft) and
 * `NodeDetail` (just one node's slice of it), since both submit the same
 * `ClusterConfig` through the same connection-sensitive-edit safety net.
 */
export function useConfigApply(reload: () => void): ConfigApply {
  const [errors, setErrors] = useState<ValidationError[]>([]);
  const [pending, setPending] = useState<PendingConfig | null>(null);
  const [busy, setBusy] = useState(false);

  // Poll for an armed rollback timer, ours or someone else's, so a page
  // reload mid-window still shows the countdown instead of hiding it.
  useEffect(() => {
    const check = () => api.pendingConfiguration().then(setPending).catch(() => {});
    check();
    const id = setInterval(check, 1000);
    return () => clearInterval(id);
  }, []);

  const validate = useCallback(async (config: ClusterConfig) => {
    try {
      const result = await api.validateConfiguration(config);
      setErrors(result.errors);
      return result.valid;
    } catch (e) {
      setErrors([{ path: '', message: e instanceof ApiError ? e.message : 'validation failed' }]);
      return false;
    }
  }, []);

  const apply = useCallback(
    async (config: ClusterConfig, rollbackAfter: string) => {
      setBusy(true);
      try {
        const ok = await validate(config);
        if (!ok) return false;
        await api.applyConfiguration(config, rollbackAfter);
        setPending(await api.pendingConfiguration());
        reload();
        return true;
      } catch (e) {
        setErrors([{ path: '', message: e instanceof ApiError ? e.message : 'apply failed' }]);
        return false;
      } finally {
        setBusy(false);
      }
    },
    [validate, reload],
  );

  const confirm = useCallback(async () => {
    await api.confirmConfiguration();
    setPending(await api.pendingConfiguration());
    reload();
  }, [reload]);

  const rollback = useCallback(async () => {
    await api.rollbackConfiguration();
    setPending(await api.pendingConfiguration());
    reload();
  }, [reload]);

  return { errors, pending, busy, validate, apply, confirm, rollback, clearErrors: () => setErrors([]) };
}
