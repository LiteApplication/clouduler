// UI-managed proxy redirects and static/external backends. Neither is part
// of cluster.yaml -- see docs/proxy.md and cl_api::redirects /
// cl_api::static_routes for why these live outside the config-draft flow and
// apply immediately instead.

import { useEffect, useState } from 'react';
import { api, ApiError } from '../api/client';
import type { RedirectRule, StaticRouteRule } from '../api/types';
import { useAuth } from '../auth/AuthContext';
import { Icon } from '../components/Icon';

const STATUS_OPTIONS: RedirectRule['status'][] = [301, 302, 307, 308];

type DraftRule = Omit<RedirectRule, 'id'>;

const emptyDraft = (network: string): DraftRule => ({
  host: '',
  network,
  path_prefix: null,
  target: '',
  preserve_path: false,
  status: 301,
});

export function Redirects() {
  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="external" />
          <div>
            <h1>Redirects</h1>
            <p className="page-sub">Rules that apply immediately, without a daemon restart.</p>
          </div>
        </div>
      </div>
      <RedirectsSection />
      <StaticBackendsSection />
    </div>
  );
}

function RedirectsSection() {
  const { isAdmin } = useAuth();
  const [rules, setRules] = useState<RedirectRule[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showForm, setShowForm] = useState(false);
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState<DraftRule>(emptyDraft(''));

  const load = () => {
    api
      .listRedirects()
      .then(setRules)
      .catch((e) => setError(e instanceof ApiError ? e.message : 'failed to load redirects'));
  };

  useEffect(load, []);

  const startCreate = () => {
    setEditing(null);
    setDraft(emptyDraft(''));
    setShowForm(true);
  };

  const startEdit = (rule: RedirectRule) => {
    setEditing(rule.id);
    setDraft({
      host: rule.host,
      network: rule.network,
      path_prefix: rule.path_prefix,
      target: rule.target,
      preserve_path: rule.preserve_path,
      status: rule.status,
    });
    setShowForm(true);
  };

  const save = async () => {
    setError(null);
    try {
      if (editing) {
        await api.updateRedirect(editing, draft);
      } else {
        await api.createRedirect(draft);
      }
      setShowForm(false);
      setEditing(null);
      load();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'failed to save redirect');
    }
  };

  const remove = async (id: string) => {
    if (!confirm('Remove this redirect?')) return;
    try {
      await api.removeRedirect(id);
      load();
    } catch (e) {
      alert(e instanceof ApiError ? e.message : 'failed to remove redirect');
    }
  };

  if (!rules) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading redirects...
      </p>
    );
  }

  return (
    <div className="section">
      <div className="section-head">
        <h2>
          {rules.length} redirect{rules.length === 1 ? '' : 's'}
        </h2>
        {isAdmin && (
          <button onClick={showForm ? () => setShowForm(false) : startCreate}>
            <Icon name={showForm ? 'close' : 'plus'} />
            {showForm ? 'Cancel' : 'New redirect'}
          </button>
        )}
      </div>
      {error && (
        <div className="banner banner-warn">
          <Icon name="alert" /> {error}
        </div>
      )}

      {showForm && (
        <div className="card section">
          <div className="field">
            <label htmlFor="redirect-host">Host</label>
            <input
              id="redirect-host"
              type="text"
              value={draft.host}
              onChange={(e) => setDraft({ ...draft, host: e.target.value })}
            />
            <span className="field-hint">A bare label gets the network's domain appended.</span>
          </div>
          <div className="field">
            <label htmlFor="redirect-network">Network</label>
            <input
              id="redirect-network"
              type="text"
              value={draft.network}
              onChange={(e) => setDraft({ ...draft, network: e.target.value })}
            />
          </div>
          <div className="field">
            <label htmlFor="redirect-prefix">Path prefix</label>
            <input
              id="redirect-prefix"
              type="text"
              value={draft.path_prefix ?? ''}
              onChange={(e) => setDraft({ ...draft, path_prefix: e.target.value || null })}
            />
          </div>
          <div className="field">
            <label htmlFor="redirect-target">Target URL</label>
            <input
              id="redirect-target"
              type="text"
              value={draft.target}
              onChange={(e) => setDraft({ ...draft, target: e.target.value })}
              placeholder="https://new.example.com"
            />
          </div>
          <div className="field">
            <label htmlFor="redirect-preserve">
              <input
                id="redirect-preserve"
                type="checkbox"
                checked={draft.preserve_path}
                onChange={(e) => setDraft({ ...draft, preserve_path: e.target.checked })}
              />
              Append the original path and query
            </label>
          </div>
          <div className="field">
            <label htmlFor="redirect-status">Status</label>
            <select
              id="redirect-status"
              value={draft.status}
              onChange={(e) => setDraft({ ...draft, status: Number(e.target.value) as RedirectRule['status'] })}
            >
              {STATUS_OPTIONS.map((s) => (
                <option key={s} value={s}>
                  {s} {s === 301 || s === 308 ? '(permanent)' : '(temporary)'}
                </option>
              ))}
            </select>
          </div>
          <button onClick={save}>
            <Icon name="check" /> {editing ? 'Save' : 'Create'}
          </button>
        </div>
      )}

      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Host</th>
              <th>Network</th>
              <th>Target</th>
              <th>Status</th>
              {isAdmin && <th>Actions</th>}
            </tr>
          </thead>
          <tbody>
            {rules.map((r) => (
              <tr key={r.id}>
                <td>
                  {r.host}
                  {r.path_prefix ? r.path_prefix : ''}
                </td>
                <td>{r.network}</td>
                <td className="mono">
                  {r.target}
                  {r.preserve_path ? '/...' : ''}
                </td>
                <td>{r.status}</td>
                {isAdmin && (
                  <td>
                    <button onClick={() => startEdit(r)}>
                      <Icon name="edit" /> Edit
                    </button>
                    <button className="danger" onClick={() => remove(r.id)}>
                      <Icon name="trash" /> Remove
                    </button>
                  </td>
                )}
              </tr>
            ))}
            {rules.length === 0 && (
              <tr>
                <td colSpan={5} className="empty-cell">
                  no redirects
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
}

type StaticRouteDraft = Omit<StaticRouteRule, 'id'>;

const emptyStaticRouteDraft = (network: string): StaticRouteDraft => ({
  host: '',
  network,
  backend: '',
  path_prefix: null,
  tls: true,
  websockets: true,
  preserve_host: false,
  max_body_size: null,
  response_timeout: null,
});

function StaticBackendsSection() {
  const { isAdmin } = useAuth();
  const [routes, setRoutes] = useState<StaticRouteRule[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showForm, setShowForm] = useState(false);
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState<StaticRouteDraft>(emptyStaticRouteDraft(''));

  const load = () => {
    api
      .listStaticRoutes()
      .then(setRoutes)
      .catch((e) => setError(e instanceof ApiError ? e.message : 'failed to load static backends'));
  };

  useEffect(load, []);

  const startCreate = () => {
    setEditing(null);
    setDraft(emptyStaticRouteDraft(''));
    setShowForm(true);
  };

  const startEdit = (route: StaticRouteRule) => {
    setEditing(route.id);
    setDraft({
      host: route.host,
      network: route.network,
      backend: route.backend,
      path_prefix: route.path_prefix,
      tls: route.tls,
      websockets: route.websockets,
      preserve_host: route.preserve_host,
      max_body_size: route.max_body_size,
      response_timeout: route.response_timeout,
    });
    setShowForm(true);
  };

  const save = async () => {
    setError(null);
    try {
      if (editing) {
        await api.updateStaticRoute(editing, draft);
      } else {
        await api.createStaticRoute(draft);
      }
      setShowForm(false);
      setEditing(null);
      load();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'failed to save static backend');
    }
  };

  const remove = async (id: string) => {
    if (!confirm('Remove this static backend?')) return;
    try {
      await api.removeStaticRoute(id);
      load();
    } catch (e) {
      alert(e instanceof ApiError ? e.message : 'failed to remove static backend');
    }
  };

  if (!routes) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading static backends...
      </p>
    );
  }

  return (
    <div className="section">
      <div className="section-head">
        <h2>
          {routes.length} static backend{routes.length === 1 ? '' : 's'}
        </h2>
        {isAdmin && (
          <button onClick={showForm ? () => setShowForm(false) : startCreate}>
            <Icon name={showForm ? 'close' : 'plus'} />
            {showForm ? 'Cancel' : 'New static backend'}
          </button>
        )}
      </div>
      <p className="field-hint">
        Routes for a service clouduler does not run itself — another box on the network, a third
        party endpoint. See docs/proxy.md.
      </p>
      {error && (
        <div className="banner banner-warn">
          <Icon name="alert" /> {error}
        </div>
      )}

      {showForm && (
        <div className="card section">
          <div className="field">
            <label htmlFor="static-route-host">Host</label>
            <input
              id="static-route-host"
              type="text"
              value={draft.host}
              onChange={(e) => setDraft({ ...draft, host: e.target.value })}
            />
            <span className="field-hint">A bare label gets the network's domain appended.</span>
          </div>
          <div className="field">
            <label htmlFor="static-route-network">Network</label>
            <input
              id="static-route-network"
              type="text"
              value={draft.network}
              onChange={(e) => setDraft({ ...draft, network: e.target.value })}
            />
          </div>
          <div className="field">
            <label htmlFor="static-route-backend">Backend</label>
            <input
              id="static-route-backend"
              type="text"
              value={draft.backend}
              onChange={(e) => setDraft({ ...draft, backend: e.target.value })}
              placeholder="192.168.1.50:9100"
            />
            <span className="field-hint">host:port. Resolved fresh on every reconcile pass.</span>
          </div>
          <div className="field">
            <label htmlFor="static-route-prefix">Path prefix</label>
            <input
              id="static-route-prefix"
              type="text"
              value={draft.path_prefix ?? ''}
              onChange={(e) => setDraft({ ...draft, path_prefix: e.target.value || null })}
            />
          </div>
          <div className="field">
            <label htmlFor="static-route-tls">
              <input
                id="static-route-tls"
                type="checkbox"
                checked={draft.tls}
                onChange={(e) => setDraft({ ...draft, tls: e.target.checked })}
              />
              TLS
            </label>
          </div>
          <div className="field">
            <label htmlFor="static-route-websockets">
              <input
                id="static-route-websockets"
                type="checkbox"
                checked={draft.websockets}
                onChange={(e) => setDraft({ ...draft, websockets: e.target.checked })}
              />
              Allow websockets
            </label>
          </div>
          <div className="field">
            <label htmlFor="static-route-preserve-host">
              <input
                id="static-route-preserve-host"
                type="checkbox"
                checked={draft.preserve_host}
                onChange={(e) => setDraft({ ...draft, preserve_host: e.target.checked })}
              />
              Preserve client Host header
            </label>
          </div>
          <div className="field">
            <label htmlFor="static-route-max-body">Max body size</label>
            <input
              id="static-route-max-body"
              type="text"
              value={draft.max_body_size ?? ''}
              onChange={(e) => setDraft({ ...draft, max_body_size: e.target.value || null })}
              placeholder="10MB"
            />
            <span className="field-hint">Blank means unlimited.</span>
          </div>
          <div className="field">
            <label htmlFor="static-route-timeout">Response timeout</label>
            <input
              id="static-route-timeout"
              type="text"
              value={draft.response_timeout ?? ''}
              onChange={(e) => setDraft({ ...draft, response_timeout: e.target.value || null })}
              placeholder="30s"
            />
            <span className="field-hint">Blank waits forever.</span>
          </div>
          <button onClick={save}>
            <Icon name="check" /> {editing ? 'Save' : 'Create'}
          </button>
        </div>
      )}

      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Host</th>
              <th>Network</th>
              <th>Backend</th>
              <th>TLS</th>
              {isAdmin && <th>Actions</th>}
            </tr>
          </thead>
          <tbody>
            {routes.map((r) => (
              <tr key={r.id}>
                <td>
                  {r.host}
                  {r.path_prefix ? r.path_prefix : ''}
                </td>
                <td>{r.network}</td>
                <td className="mono">{r.backend}</td>
                <td>{r.tls ? 'yes' : 'no'}</td>
                {isAdmin && (
                  <td>
                    <button onClick={() => startEdit(r)}>
                      <Icon name="edit" /> Edit
                    </button>
                    <button className="danger" onClick={() => remove(r.id)}>
                      <Icon name="trash" /> Remove
                    </button>
                  </td>
                )}
              </tr>
            ))}
            {routes.length === 0 && (
              <tr>
                <td colSpan={5} className="empty-cell">
                  no static backends
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
}
