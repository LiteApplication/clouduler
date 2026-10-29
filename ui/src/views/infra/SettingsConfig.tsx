// The four config tabs (General, Network, Storage, Security). They all edit
// the one draft held by ConfigDraftProvider, so a tab's badge counts only its
// own unsaved fields and the "Changes to apply" panel lists every tab's.

import { Link } from 'react-router-dom';
import { useAuth } from '../../auth/AuthContext';
import { Banner, Button, EmptyState, KV, Panel, Tag, TextField, useConfirm, useToast } from '../../ds';
import { useConfigApply, useConfigDraft, changesFor } from '../config/draft';
import { CONFIG_PAGES, nodePathTarget, pageForPath } from '../config/pages';
import type { SectionProps } from '../config/sections';
import type { DiffEntry } from '../../lib/configDiff';
import { CONFIG_SLUG_TAB } from './settingsTabs';

/** Unsaved-field count for one settings tab. */
export function tabChanges(diff: DiffEntry[], tab: string): number {
  const keys = CONFIG_PAGES.filter((p) => CONFIG_SLUG_TAB[p.slug] === tab).flatMap((p) => p.keys);
  return changesFor(diff, keys);
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
        <Panel key={slug} title={label}>
          <p className="cl-sm cl-muted">{blurb}</p>
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
      <Panel title="Changes to apply">
        <EmptyState icon="check-circle" title="In sync">
          Nothing staged. The draft matches what the cluster is running.
        </EmptyState>
      </Panel>
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
      <KV items={[[
        'Rollback window',
        <TextField
          key="rb"
          label="Seconds"
          type="number"
          inputMode="numeric"
          mono
          value={String(rollbackAfter)}
          onChange={(e) => setRollbackAfter(Math.max(5, Number(e.target.value) || 60))}
          hint={`Network-affecting changes roll back after ${rollbackAfter} seconds unless you confirm.`}
        />,
      ]]} />
    </Panel>
  );
}
