// /proxy/:tab. Routes is what the proxy serves right now (read-only); the
// other tabs hold the UI-managed rules, which apply immediately and live
// outside cluster.yaml (see docs/proxy.md).

import { useCallback, useEffect, useState, type ReactNode } from 'react';
import { Navigate, useNavigate, useParams } from 'react-router-dom';
import { api, ApiError } from '../../api/client';
import type { ProxyRoute, RedirectRule, StaticRouteRule, WireguardIngressRule } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Banner, BottomSheet, Button, PageHeader, Panel, Tabs, Tag, TextField, Toggle, useConfirm, useToast } from '../../ds';
import { daysUntil } from '../../lib/format';
import { DataTable, type Col } from './table';
import { SelectField } from '../../components/FormFields';

const TABS = [
  { id: 'routes', label: 'Routes' },
  { id: 'redirects', label: 'Redirects' },
  { id: 'static', label: 'Static backends' },
  { id: 'ingress', label: 'Ingress rules' },
];
const STATUS_OPTIONS: RedirectRule['status'][] = [301, 302, 307, 308];

function useConfigNames() {
  const [networks, setNetworks] = useState<string[]>([]);
  const [wireguard, setWireguard] = useState<string[]>([]);
  const [nodes, setNodes] = useState<string[]>([]);
  useEffect(() => {
    api
      .getConfiguration()
      .then((c) => {
        setNetworks(Object.keys(c.networks));
        setWireguard(Object.entries(c.networks).filter(([, n]) => n.type === 'wireguard').map(([k]) => k));
        setNodes(Object.keys(c.nodes));
      })
      .catch(() => {});
  }, []);
  return { networks, wireguard, nodes };
}

/** GET /api/proxy/routes is not in the client contract yet, so ask for it
 *  directly. `null` means the daemon does not have it (older build). */
async function fetchRoutes(): Promise<ProxyRoute[] | null> {
  const res = await fetch('/api/proxy/routes', { credentials: 'same-origin' });
  if (res.status === 404) return null;
  if (!res.ok) throw new ApiError(res.status, `Could not load the route list (${res.status}).`);
  return res.json();
}

export function Proxy() {
  const { tab = 'routes' } = useParams();
  const navigate = useNavigate();
  const [sheetOpen, setSheetOpen] = useState(false);
  const { isAdmin } = useAuth();
  if (!TABS.some((t) => t.id === tab)) return <Navigate to="/proxy/routes" replace />;

  const primary = {
    redirects: 'New redirect',
    static: 'New static backend',
    ingress: 'New ingress rule',
  }[tab];

  return (
    <>
      <PageHeader
        eyebrow="Network & data"
        title="Proxy"
        sub={{
          routes: 'What the reverse proxy serves right now.',
          redirects: 'Rules that apply immediately, without a daemon restart.',
          static: 'Routes for something Clouduler does not run: another box, a third-party endpoint.',
          ingress: 'Ports let in through a WireGuard tunnel, forwarded to a port on this node.',
        }[tab]}
        actions={isAdmin && primary && <Button variant="primary" icon="plus" onClick={() => setSheetOpen(true)}>{primary}</Button>}
      />
      <Tabs label="Proxy sections" items={TABS} active={tab} onChange={(id) => { setSheetOpen(false); navigate(`/proxy/${id}`); }} />
      <div className="stack" style={{ marginTop: 'var(--space-4)' }}>
        {tab === 'routes' && <RoutesTab />}
        {tab === 'redirects' && <RedirectsTab open={sheetOpen} onClose={() => setSheetOpen(false)} onOpen={() => setSheetOpen(true)} />}
        {tab === 'static' && <StaticTab open={sheetOpen} onClose={() => setSheetOpen(false)} onOpen={() => setSheetOpen(true)} />}
        {tab === 'ingress' && <IngressTab open={sheetOpen} onClose={() => setSheetOpen(false)} onOpen={() => setSheetOpen(true)} />}
      </div>
    </>
  );
}

function RoutesTab() {
  const [routes, setRoutes] = useState<ProxyRoute[] | null | undefined>(undefined);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let off = false;
    const load = () =>
      fetchRoutes().then((r) => !off && (setRoutes(r), setError(null)), (e) => !off && setError(e instanceof ApiError ? e.message : 'Could not load routes.'));
    void load();
    const id = setInterval(load, 15000);
    return () => {
      off = true;
      clearInterval(id);
    };
  }, []);

  if (error) return <Banner tone="danger">{error}</Banner>;
  if (routes === undefined) return <p className="cl-muted">Loading routes…</p>;
  if (routes === null) return <Banner tone="info" title="Route list not available">This node's daemon is older than the UI. Update it to see live routes.</Banner>;

  const bad = routes.filter((r) => r.health && !r.health.ok);
  return (
    <>
      {bad.length > 0 && (
        <Banner tone="danger" title={`${bad.length} route${bad.length === 1 ? ' returns' : 's return'} a bad gateway`}>
          {bad.map((r) => r.host + r.path).join(', ')}. Check that the service is running and listening on its port.
        </Banner>
      )}
      <Panel flush>
        <DataTable
          caption="Proxy routes"
          rows={routes.map((r) => ({ ...r, id: `${r.host}${r.path}` }))}
          empty="Nothing is published yet."
          columns={[
            { key: 'host', header: 'Host', render: (r) => <strong>{r.apex ? '@ ' : ''}{r.host}{r.path !== '/' ? r.path : ''}</strong> },
            { key: 'kind', header: 'Kind', render: (r) => <Tag>{r.kind}</Tag> },
            { key: 'to', header: 'Goes to', mono: true, render: (r) => (r.stack ? `${r.stack}.${r.service ?? ''}${r.port ? `:${r.port}` : ''}` : '-') },
            { key: 'net', header: 'Network', render: (r) => r.network ?? '-' },
            { key: 'tls', header: 'TLS', render: (r) => (r.tls ? (r.cert_not_after ? `${daysUntil(r.cert_not_after)}d left` : 'on') : 'off') },
            {
              key: 'health',
              header: 'Health',
              render: (r) =>
                r.health ? <Tag tone={r.health.ok ? 'ok' : 'danger'}>{r.health.ok ? 'ok' : `502${r.health.node ? ` on ${r.health.node}` : ''}`}</Tag> : <span className="cl-muted">-</span>,
            },
          ]}
        />
      </Panel>
    </>
  );
}

/** Shared list + sheet scaffolding for the three rule tabs. */
function RuleTab<R extends { id: string }, D>({
  open,
  onClose,
  onOpen,
  list,
  columns,
  emptyText,
  blank,
  toDraft,
  save,
  remove,
  noun,
  describe,
  form,
  disabledNote,
}: {
  open: boolean;
  onClose: () => void;
  onOpen: () => void;
  list: () => Promise<R[]>;
  columns: Col<R>[];
  emptyText: string;
  blank: () => D;
  toDraft: (r: R) => D;
  save: (id: string | null, d: D) => Promise<unknown>;
  remove: (id: string) => Promise<unknown>;
  noun: string;
  describe: (r: R) => string;
  form: (d: D, set: (d: D) => void) => ReactNode;
  disabledNote?: ReactNode;
}) {
  const { isAdmin } = useAuth();
  const confirm = useConfirm();
  const toast = useToast();
  const [rows, setRows] = useState<R[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState<D>(blank);

  const load = useCallback(() => {
    list().then(setRows, (e) => setError(e instanceof ApiError ? e.message : `Could not load ${noun}s.`));
  }, [list, noun]);
  useEffect(load, [load]);
  // The page header's primary button opens the sheet for a new rule.
  useEffect(() => {
    if (open && editing === null) setDraft(blank());
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const close = () => {
    setEditing(null);
    setFormError(null);
    onClose();
  };
  const submit = async () => {
    setFormError(null);
    try {
      await save(editing, draft);
      toast(editing ? `Saved the ${noun}.` : `Created the ${noun}.`);
      close();
      load();
    } catch (e) {
      setFormError(e instanceof ApiError ? e.message : `Could not save the ${noun}.`);
    }
  };
  const del = async (r: R) => {
    const ok = await confirm({ title: `Remove this ${noun}?`, body: describe(r), confirmLabel: `Remove ${noun}`, confirmVariant: 'danger' });
    if (!ok) return;
    try {
      await remove(r.id);
      load();
    } catch (e) {
      toast(e instanceof ApiError ? e.message : `Could not remove the ${noun}.`, { tone: 'danger' });
    }
  };

  const cols: Col<R>[] = [
    ...columns,
    ...(isAdmin
      ? [{
          key: 'act',
          header: '',
          render: (r: R) => (
            <span className="row">
              <Button iconOnly size="sm" variant="ghost" icon="edit" label={`Edit ${describe(r)}`} onClick={() => { setEditing(r.id); setDraft(toDraft(r)); onOpen(); }} />
              <Button iconOnly size="sm" variant="ghost" icon="trash" label={`Remove ${describe(r)}`} onClick={() => void del(r)} />
            </span>
          ),
        }]
      : []),
  ];

  return (
    <>
      {error && <Banner tone="danger">{error}</Banner>}
      {disabledNote}
      <Panel flush>
        {rows === null ? <p className="cl-muted" style={{ padding: 'var(--space-4)' }}>Loading…</p> : <DataTable caption={noun} rows={rows} columns={cols} empty={emptyText} />}
      </Panel>
      <BottomSheet
        open={open}
        title={editing ? `Edit ${noun}` : `New ${noun}`}
        onClose={close}
        footer={<Button variant="primary" icon="check" onClick={() => void submit()}>{editing ? 'Save' : 'Create'}</Button>}
      >
        {form(draft, setDraft)}
        {formError && <Banner tone="danger">{formError}</Banner>}
      </BottomSheet>
    </>
  );
}

type TabProps = { open: boolean; onClose: () => void; onOpen: () => void };

function RedirectsTab(p: TabProps) {
  const { networks } = useConfigNames();
  type D = Omit<RedirectRule, 'id'>;
  return (
    <RuleTab<RedirectRule, D>
      {...p}
      noun="redirect"
      emptyText="No redirects yet."
      list={api.listRedirects}
      blank={() => ({ host: '', network: networks[0] ?? '', path_prefix: null, target: '', preserve_path: false, status: 301 })}
      toDraft={({ id: _id, ...d }) => d}
      save={(id, d) => (id ? api.updateRedirect(id, d) : api.createRedirect(d))}
      remove={api.removeRedirect}
      describe={(r) => `${r.host}${r.path_prefix ?? ''}`}
      columns={[
        { key: 'host', header: 'Host', render: (r) => <strong>{r.host}{r.path_prefix ?? ''}</strong> },
        { key: 'net', header: 'Network', render: (r) => r.network },
        { key: 'to', header: 'Target', mono: true, render: (r) => `${r.target}${r.preserve_path ? '/…' : ''}` },
        { key: 'st', header: 'Status', render: (r) => r.status },
      ]}
      form={(d, set) => (
        <>
          <TextField label="Host" mono value={d.host} onChange={(e) => set({ ...d, host: e.target.value })} hint="A bare label gets the network's domain appended." />
          <SelectField id="redirect-network" label="Network" value={d.network} onChange={(network) => set({ ...d, network })} options={networks.map((n) => ({ value: n, label: n }))} />
          <TextField label="Path prefix" mono value={d.path_prefix ?? ''} onChange={(e) => set({ ...d, path_prefix: e.target.value || null })} />
          <TextField label="Target URL" mono placeholder="https://new.example.com" value={d.target} onChange={(e) => set({ ...d, target: e.target.value })} />
          <Toggle label="Append the original path and query" checked={d.preserve_path} onChange={(preserve_path) => set({ ...d, preserve_path })} />
          <SelectField
            id="redirect-status"
            label="Status"
            value={String(d.status)}
            onChange={(s) => set({ ...d, status: Number(s) as RedirectRule['status'] })}
            options={STATUS_OPTIONS.map((s) => ({ value: String(s), label: `${s} ${s === 301 || s === 308 ? '(permanent)' : '(temporary)'}` }))}
          />
        </>
      )}
    />
  );
}

function StaticTab(p: TabProps) {
  const { networks, nodes } = useConfigNames();
  type D = Omit<StaticRouteRule, 'id'>;
  return (
    <RuleTab<StaticRouteRule, D>
      {...p}
      noun="static backend"
      emptyText="No static backends yet."
      list={api.listStaticRoutes}
      blank={() => ({ host: '', network: networks[0] ?? '', node: null, backend: '', path_prefix: null, tls: true, websockets: true, preserve_host: false, max_body_size: null, response_timeout: null })}
      toDraft={({ id: _id, ...d }) => d}
      save={(id, d) => (id ? api.updateStaticRoute(id, d) : api.createStaticRoute(d))}
      remove={api.removeStaticRoute}
      describe={(r) => `${r.host}${r.path_prefix ?? ''}`}
      columns={[
        { key: 'host', header: 'Host', render: (r) => <strong>{r.host}{r.path_prefix ?? ''}</strong> },
        { key: 'net', header: 'Network', render: (r) => r.network },
        { key: 'node', header: 'Node', render: (r) => r.node ?? 'any' },
        { key: 'be', header: 'Backend', mono: true, render: (r) => r.backend },
        { key: 'tls', header: 'TLS', render: (r) => (r.tls ? 'on' : 'off') },
      ]}
      form={(d, set) => (
        <>
          <TextField label="Host" mono value={d.host} onChange={(e) => set({ ...d, host: e.target.value })} hint="A bare label gets the network's domain appended." />
          <SelectField id="static-network" label="Network" value={d.network} onChange={(network) => set({ ...d, network })} options={networks.map((n) => ({ value: n, label: n }))} />
          <SelectField id="static-node" label="Node (optional)" value={d.node ?? ''} onChange={(node) => set({ ...d, node: node || null })} options={[{ value: '', label: 'Any node' }, ...nodes.map((n) => ({ value: n, label: n }))]} />
          <TextField label="Backend" mono placeholder="192.168.1.50:9100" value={d.backend} onChange={(e) => set({ ...d, backend: e.target.value })} hint="host:port. Resolved fresh on every reconcile pass." />
          <TextField label="Path prefix" mono value={d.path_prefix ?? ''} onChange={(e) => set({ ...d, path_prefix: e.target.value || null })} />
          <Toggle label="TLS" checked={d.tls} onChange={(tls) => set({ ...d, tls })} />
          <Toggle label="Allow websockets" checked={d.websockets} onChange={(websockets) => set({ ...d, websockets })} />
          <Toggle label="Preserve client Host header" checked={d.preserve_host} onChange={(preserve_host) => set({ ...d, preserve_host })} />
          <TextField label="Max body size" mono placeholder="10MB" value={d.max_body_size ?? ''} onChange={(e) => set({ ...d, max_body_size: e.target.value || null })} hint="Blank means unlimited." />
          <TextField label="Response timeout" mono placeholder="30s" value={d.response_timeout ?? ''} onChange={(e) => set({ ...d, response_timeout: e.target.value || null })} hint="Blank waits forever." />
        </>
      )}
    />
  );
}

function IngressTab(p: TabProps) {
  const { wireguard } = useConfigNames();
  type D = Omit<WireguardIngressRule, 'id'>;
  return (
    <RuleTab<WireguardIngressRule, D>
      {...p}
      noun="ingress rule"
      emptyText="No ingress rules yet."
      list={api.listWireguardIngress}
      blank={() => ({ network: wireguard[0] ?? '', port: 0, protocol: 'tcp', target_port: 0 })}
      toDraft={({ id: _id, ...d }) => d}
      save={(id, d) => (id ? api.updateWireguardIngress(id, d) : api.createWireguardIngress(d))}
      remove={api.removeWireguardIngress}
      describe={(r) => `${r.network} ${r.protocol}/${r.port}`}
      disabledNote={wireguard.length === 0 ? <Banner tone="warn">No WireGuard networks are configured yet. Add one under Settings, Network.</Banner> : undefined}
      columns={[
        { key: 'net', header: 'Network', render: (r) => <strong>{r.network}</strong> },
        { key: 'port', header: 'Port', mono: true, render: (r) => r.port },
        { key: 'proto', header: 'Protocol', render: (r) => r.protocol.toUpperCase() },
        { key: 'target', header: 'Target port', mono: true, render: (r) => r.target_port },
      ]}
      form={(d, set) => (
        <>
          <SelectField id="wg-network" label="Network" value={d.network} onChange={(network) => set({ ...d, network })} options={wireguard.map((n) => ({ value: n, label: n }))} />
          <TextField label="Port (on the tunnel)" type="number" inputMode="numeric" mono value={String(d.port)} onChange={(e) => set({ ...d, port: Number(e.target.value) })} />
          <SelectField id="wg-protocol" label="Protocol" value={d.protocol} onChange={(protocol) => set({ ...d, protocol: protocol as 'tcp' | 'udp' })} options={[{ value: 'tcp', label: 'TCP' }, { value: 'udp', label: 'UDP' }]} />
          <TextField label="Target port (on this node)" type="number" inputMode="numeric" mono value={String(d.target_port)} onChange={(e) => set({ ...d, target_port: Number(e.target.value) })} hint="A container's published host port. Never opens the port anywhere else." />
        </>
      )}
    />
  );
}
