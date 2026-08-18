// Cluster configuration, split into subpages so an operator edits one topic
// at a time instead of scrolling one giant form. The draft lives here, above
// the subpages, so moving between them never drops an unsaved edit; the
// action bar at the bottom is likewise shared and always shows how many
// fields differ from what the cluster is running.

import { useEffect, useState } from 'react';
import { NavLink, Navigate, Route, Routes, useNavigate } from 'react-router-dom';
import { api, ApiError } from '../api/client';
import type { ClusterConfig, PendingConfig, ValidationError } from '../api/types';
import { useAuth } from '../auth/AuthContext';
import { diffConfig } from '../lib/configDiff';
import { Icon, type IconName } from '../components/Icon';
import {
  AuthSection,
  CloudflareSection,
  ClusterSection,
  DrivesSection,
  FailoverSection,
  NetworksSection,
  NodesSection,
  ProxySection,
  SecretsSection,
  WebSection,
  type SectionProps,
} from './config/sections';

function clone<T>(v: T): T {
  return JSON.parse(JSON.stringify(v));
}

interface Page {
  slug: string;
  label: string;
  icon: IconName;
  blurb: string;
  /** Top-level config keys this page owns, used for its unsaved-change count. */
  keys: string[];
  Component: (props: SectionProps) => JSX.Element;
}

const PAGES: Page[] = [
  {
    slug: 'cluster',
    label: 'Cluster',
    icon: 'cloud',
    blurb: 'Identity of the cluster itself.',
    keys: ['cluster'],
    Component: ClusterSection,
  },
  {
    slug: 'networks',
    label: 'Networks',
    icon: 'network',
    blurb: 'The internal overlay plus any private or public networks nodes are reachable on.',
    keys: ['networks'],
    Component: NetworksSection,
  },
  {
    slug: 'nodes',
    label: 'Nodes',
    icon: 'nodes',
    blurb: 'Per-node role, ports, addresses and drive mounts.',
    keys: ['nodes'],
    Component: NodesSection,
  },
  {
    slug: 'drives',
    label: 'Drives',
    icon: 'drives',
    blurb: 'How each drive is replicated or served across the cluster.',
    keys: ['drives'],
    Component: DrivesSection,
  },
  {
    slug: 'web',
    label: 'Web UI',
    icon: 'overview',
    blurb: 'Where this console listens and under which name it is published.',
    keys: ['web'],
    Component: WebSection,
  },
  {
    slug: 'proxy',
    label: 'Reverse proxy',
    icon: 'external',
    blurb: 'Published routes and the ACME certificates that front them.',
    keys: ['proxy'],
    Component: ProxySection,
  },
  {
    slug: 'failover',
    label: 'Failover',
    icon: 'activity',
    blurb: 'Timers deciding when a node is considered gone and its work moves.',
    keys: ['failover'],
    Component: FailoverSection,
  },
  {
    slug: 'secrets',
    label: 'Secrets',
    icon: 'key',
    blurb: 'Where the cluster key lives and whether a node may create one.',
    keys: ['secrets'],
    Component: SecretsSection,
  },
  {
    slug: 'auth',
    label: 'Auth',
    icon: 'lock',
    blurb: 'API keys, session lifetime and the OIDC provider.',
    keys: ['auth'],
    Component: AuthSection,
  },
  {
    slug: 'cloudflare',
    label: 'Cloudflare',
    icon: 'certificate',
    blurb: 'DNS records clouduler keeps in sync with your zones.',
    keys: ['cloudflare'],
    Component: CloudflareSection,
  },
];

export function Configuration() {
  const { isAdmin } = useAuth();
  const navigate = useNavigate();
  const [original, setOriginal] = useState<ClusterConfig | null>(null);
  const [draft, setDraft] = useState<ClusterConfig | null>(null);
  const [errors, setErrors] = useState<ValidationError[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [pending, setPending] = useState<PendingConfig | null>(null);
  const [busy, setBusy] = useState(false);

  const load = () => {
    api
      .getConfiguration()
      .then((cfg) => {
        setOriginal(cfg);
        setDraft(clone(cfg));
      })
      .catch((e) => setLoadError(e instanceof ApiError ? e.message : String(e)));
  };

  useEffect(load, []);

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

  const diff = diffConfig(original, draft);
  const countFor = (page: Page) => diff.filter((d) => page.keys.some((k) => d.path === k || d.path.startsWith(`${k}.`))).length;

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
    load();
  };

  const rollback = async () => {
    await api.rollbackConfiguration();
    setPending(await api.pendingConfiguration());
    load();
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

      <div className="config-layout">
        <nav className="config-nav" aria-label="Configuration sections">
          {PAGES.map((page) => {
            const changed = countFor(page);
            return (
              <NavLink
                key={page.slug}
                to={page.slug}
                className={({ isActive }) => `config-nav-link${isActive ? ' active' : ''}`}
              >
                <Icon name={page.icon} />
                <span className="config-nav-label">{page.label}</span>
                {changed > 0 && <span className="change-dot" aria-label={`${changed} changed`}>{changed}</span>}
              </NavLink>
            );
          })}
          <NavLink to="review" className={({ isActive }) => `config-nav-link${isActive ? ' active' : ''}`}>
            <Icon name="check-circle" />
            <span className="config-nav-label">Review</span>
            {diff.length > 0 && <span className="change-dot">{diff.length}</span>}
          </NavLink>
        </nav>

        <div className="config-page">
          <Routes>
            <Route index element={<Navigate to="cluster" replace />} />
            {PAGES.map(({ slug, label, blurb, icon, Component }) => (
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
            <button onClick={() => setDraft(clone(original))} disabled={diff.length === 0}>
              <Icon name="restart" /> Discard
            </button>
            <button className="primary" onClick={apply} disabled={busy || diff.length === 0}>
              <Icon name="upload" /> Apply
            </button>
          </div>
        </div>
      )}
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
              const top = d.path.split('.')[0];
              const page = PAGES.find((p) => p.keys.includes(top));
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
