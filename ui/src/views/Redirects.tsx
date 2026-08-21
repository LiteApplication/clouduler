// UI-managed proxy redirects and static/external backends. Neither is part
// of cluster.yaml (see docs/proxy.md and cl_api::redirects /
// cl_api::static_routes for why these live outside the config-draft flow and
// apply immediately instead.

import { useEffect, useState } from 'react';
import { api, ApiError } from '../api/client';
import type { RedirectRule, StaticRouteRule, WireguardIngressRule } from '../api/types';
import { useAuth } from '../auth/AuthContext';
import { Icon } from '../components/Icon';
import { SelectField } from '../components/FormFields';

const STATUS_OPTIONS: RedirectRule['status'][] = [301, 302, 307, 308];

function useNetworkNames(): string[] {
  const [names, setNames] = useState<string[]>([]);
  useEffect(() => {
    api
      .getConfiguration()
      .then((config) => setNames(Object.keys(config.networks)))
      .catch(() => setNames([]));
  }, []);
  return names;
}

function useWireguardNetworkNames(): string[] {
  const [names, setNames] = useState<string[]>([]);
  useEffect(() => {
    api
      .getConfiguration()
      .then((config) =>
        setNames(Object.entries(config.networks).filter(([, n]) => n.type === 'wireguard').map(([name]) => name)),
      )
      .catch(() => setNames([]));
  }, []);
  return names;
}

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
      <WireguardIngressSection />
    </div>
  );
}

function RedirectsSection() {
  const { isAdmin } = useAuth();
  const networkNames = useNetworkNames();
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
    setDraft(emptyDraft(networkNames[0] ?? ''));
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
          <SelectField
            id="redirect-network"
            label="Network"
            value={draft.network}
            onChange={(network) => setDraft({ ...draft, network })}
            options={networkNames.map((n) => ({ value: n, label: n }))}
          />
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
  const networkNames = useNetworkNames();
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
    setDraft(emptyStaticRouteDraft(networkNames[0] ?? ''));
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
          <SelectField
            id="static-route-network"
            label="Network"
            value={draft.network}
            onChange={(network) => setDraft({ ...draft, network })}
            options={networkNames.map((n) => ({ value: n, label: n }))}
          />
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

type WireguardIngressDraft = Omit<WireguardIngressRule, 'id'>;

const emptyWireguardIngressDraft = (network: string): WireguardIngressDraft => ({
  network,
  port: 0,
  protocol: 'tcp',
  target_port: 0,
});

function WireguardIngressSection() {
  const { isAdmin } = useAuth();
  const networkNames = useWireguardNetworkNames();
  const [rules, setRules] = useState<WireguardIngressRule[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showForm, setShowForm] = useState(false);
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState<WireguardIngressDraft>(emptyWireguardIngressDraft(''));

  const load = () => {
    api
      .listWireguardIngress()
      .then(setRules)
      .catch((e) => setError(e instanceof ApiError ? e.message : 'failed to load wireguard ingress rules'));
  };

  useEffect(load, []);

  const startCreate = () => {
    setEditing(null);
    setDraft(emptyWireguardIngressDraft(networkNames[0] ?? ''));
    setShowForm(true);
  };

  const startEdit = (rule: WireguardIngressRule) => {
    setEditing(rule.id);
    setDraft({ network: rule.network, port: rule.port, protocol: rule.protocol, target_port: rule.target_port });
    setShowForm(true);
  };

  const save = async () => {
    setError(null);
    try {
      if (editing) {
        await api.updateWireguardIngress(editing, draft);
      } else {
        await api.createWireguardIngress(draft);
      }
      setShowForm(false);
      setEditing(null);
      load();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'failed to save wireguard ingress rule');
    }
  };

  const remove = async (id: string) => {
    if (!confirm('Remove this wireguard ingress rule?')) return;
    try {
      await api.removeWireguardIngress(id);
      load();
    } catch (e) {
      alert(e instanceof ApiError ? e.message : 'failed to remove wireguard ingress rule');
    }
  };

  if (!rules) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading wireguard ingress rules...
      </p>
    );
  }

  return (
    <div className="section">
      <div className="section-head">
        <h2>
          {rules.length} wireguard ingress rule{rules.length === 1 ? '' : 's'}
        </h2>
        {isAdmin && networkNames.length > 0 && (
          <button onClick={showForm ? () => setShowForm(false) : startCreate}>
            <Icon name={showForm ? 'close' : 'plus'} />
            {showForm ? 'Cancel' : 'New rule'}
          </button>
        )}
      </div>
      <p className="field-hint">
        Ports let in through a wireguard network's tunnel, forwarded to a port on this node, normally an
        already-published docker port bind. Never opens the port anywhere else.
      </p>
      {networkNames.length === 0 && (
        <div className="banner banner-warn">
          <Icon name="alert" /> No wireguard networks configured yet.
        </div>
      )}
      {error && (
        <div className="banner banner-warn">
          <Icon name="alert" /> {error}
        </div>
      )}

      {showForm && (
        <div className="card section">
          <SelectField
            id="wg-ingress-network"
            label="Network"
            value={draft.network}
            onChange={(network) => setDraft({ ...draft, network })}
            options={networkNames.map((n) => ({ value: n, label: n }))}
          />
          <div className="field">
            <label htmlFor="wg-ingress-port">Port (on the tunnel)</label>
            <input
              id="wg-ingress-port"
              type="number"
              value={draft.port}
              onChange={(e) => setDraft({ ...draft, port: Number(e.target.value) })}
            />
          </div>
          <SelectField
            id="wg-ingress-protocol"
            label="Protocol"
            value={draft.protocol}
            onChange={(protocol) => setDraft({ ...draft, protocol: protocol as 'tcp' | 'udp' })}
            options={[
              { value: 'tcp', label: 'TCP' },
              { value: 'udp', label: 'UDP' },
            ]}
          />
          <div className="field">
            <label htmlFor="wg-ingress-target">Target port (on this node)</label>
            <input
              id="wg-ingress-target"
              type="number"
              value={draft.target_port}
              onChange={(e) => setDraft({ ...draft, target_port: Number(e.target.value) })}
            />
            <span className="field-hint">A docker container's published host port. Same as Port when unchanged.</span>
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
              <th>Network</th>
              <th>Port</th>
              <th>Protocol</th>
              <th>Target port</th>
              {isAdmin && <th>Actions</th>}
            </tr>
          </thead>
          <tbody>
            {rules.map((r) => (
              <tr key={r.id}>
                <td>{r.network}</td>
                <td className="mono">{r.port}</td>
                <td>{r.protocol}</td>
                <td className="mono">{r.target_port}</td>
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
                  no wireguard ingress rules
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
}
