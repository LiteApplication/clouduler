// The four config tabs (General, Network, Storage, Security). They all edit
// the one draft held by ConfigDraftProvider, so a tab's badge counts only its
// own unsaved fields and the "Changes to apply" panel lists every tab's.
// Each page is one panel with a one-line purpose under its title.

import { useEffect } from 'react';
import { Link, useSearchParams } from 'react-router-dom';
import { useAuth } from '../../auth/AuthContext';
import { Banner, Button, Icon, Panel, Tag, TextField, useConfirm, useToast } from '../../ds';
import { useConfigApply, useConfigDraft } from '../config/draft';
import { CONFIG_PAGES, nodePathTarget, pageForPath } from '../config/pages';
import type { SectionProps } from '../config/sections';
import type { DiffEntry } from '../../lib/configDiff';
import { CONFIG_SLUG_TAB } from './settingsTabs';

/** Unsaved-field count for one settings tab. */
export function tabChanges(diff: DiffEntry[], tab: string): number {
  // Attribute each diff to its single owning page, so `proxy.auth.*` counts
  // under Service login only, not also under Proxy.
  return diff.filter((d) => {
    const page = pageForPath(d.path);
    return !!page && CONFIG_SLUG_TAB[page.slug] === tab;
  }).length;
}

export function useSettingsConfig() {
  const draftCtx = useConfigDraft();
  const apply = useConfigApply(draftCtx.reload);
  return { ...draftCtx, ...apply };
}

export type SettingsConfig = ReturnType<typeof useSettingsConfig>;

/** The header button's label follows whether a rollback timer is running. */
export function primaryAction(cfg: SettingsConfig) {
  const armed = !!cfg.pending?.armed;
  return {
    label: armed ? 'Confirm config' : 'Apply changes',
    icon: 'check' as const,
    disabled: armed ? false : cfg.busy || cfg.diff.length === 0,
  };
}

export function ConfigTab({ tab, cfg, rollbackAfter, setRollbackAfter }: {
  tab: string;
  cfg: SettingsConfig;
  rollbackAfter: number;
  setRollbackAfter: (n: number) => void;
}) {
  const { isAdmin } = useAuth();
  const toast = useToast();
  const confirm = useConfirm();
  const [params] = useSearchParams();
  const wanted = params.get('page');
  const ready = !!cfg.original && !!cfg.draft;
  // /configuration/<slug> redirects here with ?page=<slug>: bring that panel into view.
  useEffect(() => {
    if (wanted && ready) document.getElementById(`cfg-${wanted}`)?.scrollIntoView({ block: 'start' });
  }, [wanted, ready, tab]);
  const { original, draft, setDraft, discard, diff, loadError, errors, pending, validate, rollback } = cfg;

  if (loadError) return <Banner tone="danger" title="Could not load the configuration">{loadError}</Banner>;
  if (!original || !draft) return <p className="cl-muted">Loading configuration…</p>;

  const props: SectionProps = { draft, setDraft, disabled: !isAdmin, original };
  const pages = CONFIG_PAGES.filter((p) => CONFIG_SLUG_TAB[p.slug] === tab);

  const doRollback = async () => {
    if (await confirm({ title: 'Roll back now?', body: 'The previous config comes back immediately.', confirmLabel: 'Roll back now', confirmVariant: 'danger' })) {
      await rollback();
      toast('Rolled back to the previous config.');
    }
  };

  return (
    <div className="stack cfg">
      {pending?.armed && (
        <Banner tone="warn" title="Config change waiting for you" action="Roll back now" onAction={doRollback}>
          <span className="pending-countdown">{pending.seconds_left ?? '?'}s</span> left to confirm. Without a confirm,
          Clouduler returns to the previous config so you cannot lock yourself out.
        </Banner>
      )}
      {errors.length > 0 && (
        <Banner tone="danger" title="Fix these before applying">
          {errors.map((e, i) => (
            <span key={i} style={{ display: 'block' }}>
              {e.path && <code>{e.path}</code>}
              {e.path && ': '}
              {e.message}
            </span>
          ))}
        </Banner>
      )}
      {pages.map(({ slug, label, blurb, Component }) => (
        <Panel key={slug} title={label} className="settings__panel">
          <span id={`cfg-${slug}`} className="settings__anchor" />
          <p className="sub settings__blurb">{blurb}</p>
          <Component {...props} />
        </Panel>
      ))}
      <ChangesPanel
        diff={diff}
        isAdmin={isAdmin}
        busy={cfg.busy}
        rollbackAfter={rollbackAfter}
        setRollbackAfter={setRollbackAfter}
        onValidate={async () => {
          if (await validate(draft)) toast('Config is valid.');
        }}
        onDiscard={discard}
      />
    </div>
  );
}

function ChangesPanel({ diff, isAdmin, busy, rollbackAfter, setRollbackAfter, onValidate, onDiscard }: {
  diff: DiffEntry[];
  isAdmin: boolean;
  busy: boolean;
  rollbackAfter: number;
  setRollbackAfter: (n: number) => void;
  onValidate: () => void;
  onDiscard: () => void;
}) {
  if (diff.length === 0) {
    return (
      <p className="settings__sync">
        <Icon name="check-circle" size={18} /> In sync. Nothing staged; the cluster runs what you see here.
      </p>
    );
  }
  return (
    <Panel
      title={<>Changes to apply <Tag tone="brand">{diff.length} field{diff.length > 1 ? 's' : ''}</Tag></>}
      actions={
        isAdmin && (
          <>
            <Button size="sm" onClick={onValidate} disabled={busy}>Validate config</Button>
            <Button size="sm" variant="ghost" onClick={onDiscard}>Discard edits</Button>
          </>
        )
      }
    >
      <div className="diff" role="region" aria-label="Config diff" tabIndex={0}>
        {diff.flatMap((d) => {
          const page = pageForPath(d.path);
          const target = page ? `/settings/${CONFIG_SLUG_TAB[page.slug]}` : nodePathTarget(d.path);
          return [
            <div key={`${d.path}-c`} className="diff__l diff__l--ctx">
              <span>  </span>
              <span>{target ? <Link to={target}>{d.path}</Link> : d.path}:</span>
            </div>,
            <div key={`${d.path}-d`} className="diff__l diff__l--del"><span>- </span><span>{d.before}</span></div>,
            <div key={`${d.path}-a`} className="diff__l diff__l--add"><span>+ </span><span>{d.after}</span></div>,
          ];
        })}
      </div>
      <TextField
        label="Confirm within (seconds)"
        type="number"
        inputMode="numeric"
        mono
        value={String(rollbackAfter)}
        onChange={(e) => setRollbackAfter(Math.max(5, Number(e.target.value) || 60))}
        hint="Network changes roll back after this long unless you confirm."
      />
    </Panel>
  );
}
