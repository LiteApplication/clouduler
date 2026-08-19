// Cluster configuration, split into subpages so an operator edits one topic
// at a time instead of scrolling one giant form. The subpage list lives in the
// app sidebar (see Layout), and the draft they all edit lives in
// ConfigDraftProvider above both, so moving between them never drops an
// unsaved edit. The action bar sits at the top of the page, under the header,
// and always shows how many fields differ from what the cluster is running.

import { useEffect, useState } from 'react';
import { Navigate, Route, Routes, useNavigate } from 'react-router-dom';
import { api, ApiError } from '../api/client';
import type { PendingConfig, ValidationError } from '../api/types';
import { useAuth } from '../auth/AuthContext';
import { Icon } from '../components/Icon';
import { useConfigDraft } from './config/draft';
import { CONFIG_PAGES, pageForPath } from './config/pages';
import type { SectionProps } from './config/sections';

export function Configuration() {
  const { isAdmin } = useAuth();
  const navigate = useNavigate();
  const { original, draft, setDraft, discard, reload, diff, loadError } = useConfigDraft();
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

  const validate = async () => {
    try {
      const result = await api.validateConfiguration(draft);
      setErrors(result.errors);
      return result.valid;
    } catch (e) {
      setErrors([{ path: '', message: e instanceof ApiError ? e.message : 'validation failed' }]);
      return false;
    }
  };

  const apply = async () => {
    setBusy(true);
    try {
      const ok = await validate();
      if (!ok) {
        setBusy(false);
        return;
      }
      await api.applyConfiguration(draft, '60s');
      setPending(await api.pendingConfiguration());
    } catch (e) {
      setErrors([{ path: '', message: e instanceof ApiError ? e.message : 'apply failed' }]);
    } finally {
      setBusy(false);
    }
  };

  const confirm = async () => {
    await api.confirmConfiguration();
    setPending(await api.pendingConfiguration());
    reload();
  };

  const rollback = async () => {
    await api.rollbackConfiguration();
    setPending(await api.pendingConfiguration());
    reload();
  };

  const sectionProps: SectionProps = { draft, setDraft, disabled: !isAdmin };

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
            <button onClick={validate} disabled={busy}>
              <Icon name="check" /> Validate
            </button>
            <button onClick={() => navigate('/configuration/review')} disabled={diff.length === 0}>
              <Icon name="file" /> Review
            </button>
            <button onClick={discard} disabled={diff.length === 0}>
              <Icon name="restart" /> Discard
            </button>
            <button className="primary" onClick={apply} disabled={busy || diff.length === 0}>
              <Icon name="upload" /> Apply
            </button>
          </div>
        </div>
      )}

      {pending?.armed && (
        <div className="banner banner-warn pending-banner" role="alert">
          <div className="flex-row">
            <Icon name="clock" />
            <strong>A config change is pending confirmation.</strong>
          </div>
          <span className="countdown">{pending.seconds_left ?? '?'}s</span>
          <span>
            If you do not confirm before the timer runs out, the previous config is restored automatically. This
            protects you from editing away the network you are connected over.
          </span>
          <div className="btn-row">
            <button className="primary" onClick={confirm}>
              <Icon name="check" /> Confirm new config
            </button>
            <button className="danger" onClick={rollback}>
              <Icon name="restart" /> Roll back now
            </button>
          </div>
        </div>
      )}

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
            element={<ReviewPage diff={diff} onGo={(slug) => navigate(`/configuration/${slug}`)} />}
          />
          <Route path="*" element={<Navigate to="cluster" replace />} />
        </Routes>

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
      </div>
    </div>
  );
}

function ReviewPage({
  diff,
  onGo,
}: {
  diff: { path: string; before: string; after: string }[];
  onGo: (slug: string) => void;
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
        Applying arms a 60 second rollback timer: confirm afterwards, or the previous config comes back on its own.
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
              return (
                <tr key={d.path}>
                  <td className="mono">{d.path}</td>
                  <td className="diff-remove wrap">{d.before}</td>
                  <td className="diff-add wrap">{d.after}</td>
                  <td>
                    {page && (
                      <button className="ghost" onClick={() => onGo(page.slug)} aria-label={`Edit ${d.path}`}>
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
