// Cluster configuration, split into subpages so an operator edits one topic
// at a time instead of scrolling one giant form. The subpage list lives in the
// app sidebar (see Layout), and the draft they all edit lives in
// ConfigDraftProvider above both, so moving between them never drops an
// unsaved edit. The action bar sits at the top of the page, under the header,
// and always shows how many fields differ from what the cluster is running.
//
// Per-node config (role, ports, addresses, drive mounts) lives on that node's
// own detail page instead of a subpage here (see `NodeDetail`), since it
// reuses the same draft and apply flow (`useConfigApply`) but does not need a
// tab of its own once every node already has one under Nodes in the sidebar.

import { Navigate, Route, Routes, useNavigate } from 'react-router-dom';
import { useAuth } from '../auth/AuthContext';
import { Icon } from '../components/Icon';
import { useConfigApply, useConfigDraft } from './config/draft';
import { PendingBanner } from './config/PendingBanner';
import { CONFIG_PAGES, nodePathTarget, pageForPath } from './config/pages';
import type { SectionProps } from './config/sections';
import { useState } from 'react';

export function Configuration() {
  const { isAdmin } = useAuth();
  const navigate = useNavigate();
  const { original, draft, setDraft, discard, reload, diff, loadError } = useConfigDraft();
  const { errors, pending, busy, validate, apply, confirm, rollback } = useConfigApply(reload);
  const [rollbackAfter, setRollbackAfter] = useState(60);

  if (loadError) {
    return (
      <p className="field-error">
        <Icon name="alert-circle" /> {loadError}
      </p>
    );
  }
  if (!original || !draft) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading configuration...
      </p>
    );
  }

  const sectionProps: SectionProps = { draft, setDraft, disabled: !isAdmin, original };

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="settings" />
          <div>
            <h1>Configuration</h1>
            <p className="page-sub">
              {diff.length === 0
                ? 'In sync with the running cluster config.'
                : `${diff.length} unapplied change${diff.length > 1 ? 's' : ''}`}
            </p>
          </div>
        </div>
      </div>

      {isAdmin && (
        <div className="action-bar">
          <span className="muted action-bar-status">
            {diff.length === 0 ? (
              <>
                <Icon name="check-circle" /> No changes
              </>
            ) : (
              <>
                <Icon name="edit" /> {diff.length} change{diff.length > 1 ? 's' : ''} staged
              </>
            )}
          </span>
          <div className="btn-row">
            <label className="rollback-after" title="If a network-affecting change (networks, nodes, web ui, reverse proxy, Cloudflare) isn't confirmed within this window, it rolls back automatically.">
              <Icon name="clock" /> Rollback after
              <input
                type="number"
                min={5}
                max={3600}
                value={rollbackAfter}
                onChange={(e) => setRollbackAfter(Math.max(5, Number(e.target.value) || 60))}
                disabled={busy}
              />
              s
            </label>
            <button onClick={() => validate(draft)} disabled={busy}>
              <Icon name="check" /> Validate
            </button>
            <button onClick={() => navigate('/configuration/review')} disabled={diff.length === 0}>
              <Icon name="file" /> Review
            </button>
            <button onClick={discard} disabled={diff.length === 0}>
              <Icon name="restart" /> Discard
            </button>
            <button className="primary" onClick={() => apply(draft, `${rollbackAfter}s`)} disabled={busy || diff.length === 0}>
              <Icon name="upload" /> Apply
            </button>
          </div>
        </div>
      )}

      {errors.length > 0 && (
        <div className="banner banner-bad" role="alert" style={{ marginTop: 16 }}>
          <Icon name="alert" />
          <ul style={{ margin: 0, paddingLeft: 18 }}>
            {errors.map((e, i) => (
              <li key={i}>
                {e.path && <span className="mono">{e.path}</span>}
                {e.path && ': '}
                {e.message}
              </li>
            ))}
          </ul>
        </div>
      )}

      {pending?.armed && <PendingBanner pending={pending} onConfirm={confirm} onRollback={rollback} />}

      <div className="config-page">
        <Routes>
          <Route index element={<Navigate to="cluster" replace />} />
          {CONFIG_PAGES.map(({ slug, label, blurb, icon, Component }) => (
            <Route
              key={slug}
              path={slug}
              element={
                <>
                  <div className="section-head">
                    <h2>
                      <Icon name={icon} /> {label}
                    </h2>
                  </div>
                  <p className="page-sub" style={{ marginBottom: 16 }}>
                    {blurb}
                  </p>
                  <Component {...sectionProps} />
                </>
              }
            />
          ))}
          <Route
            path="review"
            element={<ReviewPage diff={diff} rollbackAfter={rollbackAfter} onGo={(path) => navigate(path)} />}
          />
          <Route path="*" element={<Navigate to="cluster" replace />} />
        </Routes>
      </div>
    </div>
  );
}

function ReviewPage({
  diff,
  rollbackAfter,
  onGo,
}: {
  diff: { path: string; before: string; after: string }[];
  rollbackAfter: number;
  onGo: (path: string) => void;
}) {
  if (diff.length === 0) {
    return (
      <>
        <div className="section-head">
          <h2>
            <Icon name="check-circle" /> Review
          </h2>
        </div>
        <div className="banner banner-good">
          <Icon name="check-circle" /> The draft matches the running configuration.
        </div>
      </>
    );
  }

  return (
    <>
      <div className="section-head">
        <h2>
          <Icon name="check-circle" /> Review {diff.length} change{diff.length > 1 ? 's' : ''}
        </h2>
      </div>
      <p className="page-sub" style={{ marginBottom: 16 }}>
        If any change touches networks, nodes, the web ui, the reverse proxy or Cloudflare, applying arms a{' '}
        {rollbackAfter} second rollback timer: confirm afterwards, or the previous config comes back on its own.
        Everything else takes effect immediately.
      </p>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Field</th>
              <th>Before</th>
              <th>After</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {diff.map((d) => {
              const page = pageForPath(d.path);
              const target = page ? `/configuration/${page.slug}` : nodePathTarget(d.path);
              return (
                <tr key={d.path}>
                  <td className="mono">{d.path}</td>
                  <td className="diff-remove wrap">{d.before}</td>
                  <td className="diff-add wrap">{d.after}</td>
                  <td>
                    {target && (
                      <button className="ghost" onClick={() => onGo(target)} aria-label={`Edit ${d.path}`}>
                        <Icon name="edit" />
                      </button>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </>
  );
}
