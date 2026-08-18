import { useEffect, useState } from 'react';
import { api, ApiError } from '../api/client';
import type {
  ClusterConfig,
  NetworkConfig,
  NetworkKind,
  NodeAddress,
  NodeConfig,
  PendingConfig,
  PublicAddress,
  ValidationError,
} from '../api/types';
import { useAuth } from '../auth/AuthContext';
import { diffConfig } from '../lib/configDiff';
import {
  CheckboxField,
  CidrField,
  DurationField,
  IpField,
  KeyValueList,
  PortField,
  SelectField,
  TextField,
} from '../components/FormFields';

function clone<T>(v: T): T {
  return JSON.parse(JSON.stringify(v));
}

export function Configuration() {
  const { isAdmin } = useAuth();
  const [original, setOriginal] = useState<ClusterConfig | null>(null);
  const [draft, setDraft] = useState<ClusterConfig | null>(null);
  const [errors, setErrors] = useState<ValidationError[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [showDiff, setShowDiff] = useState(false);
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

  if (loadError) return <p className="field-error">{loadError}</p>;
  if (!original || !draft) return <p className="muted">Loading configuration...</p>;

  const diff = diffConfig(original, draft);

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
      setShowDiff(false);
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

  return (
    <div>
      <div className="page-header">
        <h1>Configuration</h1>
      </div>

      {pending?.armed && (
        <div className="banner banner-warn" role="alert" style={{ flexDirection: 'column', alignItems: 'flex-start' }}>
          <strong>A config change is pending confirmation.</strong>
          <span className="countdown">{pending.seconds_left ?? '?'}s</span>
          <span>
            If you do not confirm before the timer runs out, the previous config is restored automatically. This
            protects you from editing away the network you are connected over.
          </span>
          <div className="btn-row" style={{ marginTop: 8 }}>
            <button className="primary" onClick={confirm}>
              Confirm new config
            </button>
            <button className="danger" onClick={rollback}>
              Roll back now
            </button>
          </div>
        </div>
      )}

      <ClusterSection draft={draft} setDraft={setDraft} disabled={!isAdmin} />
      <NetworksSection draft={draft} setDraft={setDraft} disabled={!isAdmin} />
      <DrivesSection draft={draft} setDraft={setDraft} disabled={!isAdmin} />
      <NodesSection draft={draft} setDraft={setDraft} disabled={!isAdmin} />
      <WebSection draft={draft} setDraft={setDraft} disabled={!isAdmin} />
      <FailoverSection draft={draft} setDraft={setDraft} disabled={!isAdmin} />
      <SecretsSection draft={draft} setDraft={setDraft} disabled={!isAdmin} />
      <AuthSection draft={draft} setDraft={setDraft} disabled={!isAdmin} />
      <CloudflareSection draft={draft} setDraft={setDraft} disabled={!isAdmin} />
      <ProxySection draft={draft} setDraft={setDraft} disabled={!isAdmin} />

      {errors.length > 0 && (
        <div className="banner banner-bad" role="alert">
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

      {isAdmin && (
        <div className="btn-row section">
          <button onClick={validate} disabled={busy}>
            Validate
          </button>
          <button onClick={() => setShowDiff((v) => !v)} disabled={diff.length === 0}>
            {showDiff ? 'Hide diff' : `Review diff (${diff.length})`}
          </button>
          <button className="primary" onClick={apply} disabled={busy || diff.length === 0}>
            Apply
          </button>
          <button onClick={() => setDraft(clone(original))} disabled={diff.length === 0}>
            Discard changes
          </button>
        </div>
      )}

      {showDiff && (
        <div className="section table-wrap">
          <table>
            <thead>
              <tr>
                <th>Field</th>
                <th>Before</th>
                <th>After</th>
              </tr>
            </thead>
            <tbody>
              {diff.map((d) => (
                <tr key={d.path}>
                  <td className="mono">{d.path}</td>
                  <td className="diff-remove wrap">{d.before}</td>
                  <td className="diff-add wrap">{d.after}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

interface SectionProps {
  draft: ClusterConfig;
  setDraft: (c: ClusterConfig) => void;
  disabled: boolean;
}

function ClusterSection({ draft, setDraft, disabled }: SectionProps) {
  return (
    <fieldset>
      <legend>Cluster</legend>
      <TextField
        id="cluster-name"
        label="Name"
        value={draft.cluster.name}
        onChange={(name) => setDraft({ ...draft, cluster: { ...draft.cluster, name } })}
        disabled={disabled}
      />
    </fieldset>
  );
}

function NetworksSection({ draft, setDraft, disabled }: SectionProps) {
  const entries = Object.entries(draft.networks);

  const updateNetwork = (name: string, patch: Partial<NetworkConfig>) => {
    setDraft({ ...draft, networks: { ...draft.networks, [name]: { ...draft.networks[name], ...patch } } });
  };

  const addNetwork = () => {
    let name = 'network';
    let i = 1;
    while (draft.networks[name]) name = `network${i++}`;
    setDraft({ ...draft, networks: { ...draft.networks, [name]: { type: 'private', subnet: null, domain: null } } });
  };

  const removeNetwork = (name: string) => {
    const next = { ...draft.networks };
    delete next[name];
    setDraft({ ...draft, networks: next });
  };

  return (
    <fieldset>
      <legend>Networks</legend>
      {entries.map(([name, net]) => (
        <div className="card section" key={name}>
          <div className="page-header">
            <h3>{name}</h3>
            {!disabled && net.type !== 'internal' && <button onClick={() => removeNetwork(name)}>Remove</button>}
          </div>
          <SelectField
            id={`net-${name}-type`}
            label="Type"
            value={net.type}
            onChange={(type) => updateNetwork(name, { type: type as NetworkKind })}
            options={[
              { value: 'internal', label: 'Internal' },
              { value: 'private', label: 'Private' },
              { value: 'public', label: 'Public' },
            ]}
            disabled={disabled}
          />
          {net.type !== 'public' && (
            <CidrField
              id={`net-${name}-subnet`}
              label="Subnet"
              value={net.subnet ?? ''}
              onChange={(subnet) => updateNetwork(name, { subnet: subnet || null })}
              disabled={disabled}
            />
          )}
          <TextField
            id={`net-${name}-domain`}
            label="Domain"
            value={net.domain ?? ''}
            onChange={(domain) => updateNetwork(name, { domain: domain || null })}
            hint="Records under this domain resolve to node addresses on this network."
            disabled={disabled}
          />
        </div>
      ))}
      {!disabled && <button onClick={addNetwork}>Add network</button>}
    </fieldset>
  );
}

function DrivesSection({ draft, setDraft, disabled }: SectionProps) {
  const entries = Object.entries(draft.drives);

  const update = (name: string, patch: Partial<ClusterConfig['drives'][string]>) => {
    setDraft({ ...draft, drives: { ...draft.drives, [name]: { ...draft.drives[name], ...patch } } });
  };

  const add = () => {
    let name = 'drive';
    let i = 1;
    while (draft.drives[name]) name = `drive${i++}`;
    setDraft({ ...draft, drives: { ...draft.drives, [name]: { sync_interval: '15m', mode: 'replicated', served_by: null } } });
  };

  const remove = (name: string) => {
    const next = { ...draft.drives };
    delete next[name];
    setDraft({ ...draft, drives: next });
  };

  return (
    <fieldset>
      <legend>Drives</legend>
      {entries.map(([name, drive]) => (
        <div className="card section" key={name}>
          <div className="page-header">
            <h3>{name}</h3>
            {!disabled && <button onClick={() => remove(name)}>Remove</button>}
          </div>
          <SelectField
            id={`drive-${name}-mode`}
            label="Mode"
            value={drive.mode}
            onChange={(mode) => update(name, { mode: mode as 'replicated' | 'served' })}
            options={[
              { value: 'replicated', label: 'Replicated' },
              { value: 'served', label: 'Served' },
            ]}
            disabled={disabled}
          />
          {drive.mode === 'replicated' ? (
            <DurationField
              id={`drive-${name}-interval`}
              label="Sync interval"
              value={drive.sync_interval}
              onChange={(sync_interval) => update(name, { sync_interval })}
              disabled={disabled}
            />
          ) : (
            <SelectField
              id={`drive-${name}-servedby`}
              label="Served by"
              value={drive.served_by ?? ''}
              onChange={(served_by) => update(name, { served_by: served_by || null })}
              options={[{ value: '', label: '(choose a node)' }, ...Object.keys(draft.nodes).map((n) => ({ value: n, label: n }))]}
              disabled={disabled}
            />
          )}
        </div>
      ))}
      {!disabled && <button onClick={add}>Add drive</button>}
    </fieldset>
  );
}

function NodesSection({ draft, setDraft, disabled }: SectionProps) {
  const entries = Object.entries(draft.nodes);

  const update = (name: string, patch: Partial<NodeConfig>) => {
    setDraft({ ...draft, nodes: { ...draft.nodes, [name]: { ...draft.nodes[name], ...patch } } });
  };

  const remove = (name: string) => {
    const next = { ...draft.nodes };
    delete next[name];
    setDraft({ ...draft, nodes: next });
  };

  return (
    <fieldset>
      <legend>Nodes</legend>
      {entries.map(([name, node]) => (
        <NodeCard key={name} name={name} node={node} networks={draft.networks} onChange={(patch) => update(name, patch)} onRemove={() => remove(name)} disabled={disabled} />
      ))}
    </fieldset>
  );
}

function NodeCard({
  name,
  node,
  networks,
  onChange,
  onRemove,
  disabled,
}: {
  name: string;
  node: NodeConfig;
  networks: ClusterConfig['networks'];
  onChange: (patch: Partial<NodeConfig>) => void;
  onRemove: () => void;
  disabled: boolean;
}) {
  const drivePairs: [string, string][] = Object.entries(node.drives);

  const setAddress = (netName: string, addr: NodeAddress | null) => {
    const nextNetwork = { ...node.network };
    if (addr === null) delete nextNetwork[netName];
    else nextNetwork[netName] = addr;
    onChange({ network: nextNetwork });
  };

  return (
    <div className="card section">
      <div className="page-header">
        <h3>{name}</h3>
        {!disabled && <button className="danger" onClick={onRemove}>Remove node</button>}
      </div>
      <SelectField
        id={`node-${name}-role`}
        label="Role"
        value={node.role}
        onChange={(role) => onChange({ role: role as 'manager' | 'worker' })}
        options={[
          { value: 'manager', label: 'Manager' },
          { value: 'worker', label: 'Worker' },
        ]}
        disabled={disabled}
      />
      <PortField
        id={`node-${name}-port`}
        label="RPC port"
        value={node.rpc_port}
        onChange={(rpc_port) => onChange({ rpc_port })}
        disabled={disabled}
      />
      <TextField
        id={`node-${name}-datadir`}
        label="Data dir override"
        value={node.data_dir ?? ''}
        onChange={(data_dir) => onChange({ data_dir: data_dir || null })}
        disabled={disabled}
      />
      {/* A node that joined an existing cluster is set to "never", because a
          second cluster key would only seal secrets nothing else can open. */}
      <SelectField
        id={`node-${name}-genkey`}
        label="Create the cluster key if missing"
        value={node.generate_secret_key === null || node.generate_secret_key === undefined ? 'inherit' : String(node.generate_secret_key)}
        onChange={(choice) =>
          onChange({ generate_secret_key: choice === 'inherit' ? null : choice === 'true' })
        }
        options={[
          { value: 'inherit', label: 'Follow the cluster setting' },
          { value: 'true', label: 'Yes, this node may create it' },
          { value: 'false', label: 'Never, it can only be given the key' },
        ]}
        disabled={disabled}
      />

      <h4>Network addresses</h4>
      {Object.entries(networks).map(([netName, net]) => {
        const addr = node.network[netName];
        if (net.type === 'public') {
          const pub = (typeof addr === 'object' ? addr : {}) as Partial<PublicAddress>;
          const update = (patch: Partial<PublicAddress>) => {
            const merged: PublicAddress = {
              ipv4: pub.ipv4 ?? null,
              ipv6: pub.ipv6 ?? null,
              domain: pub.domain ?? null,
              ...patch,
            };
            const empty = !merged.ipv4 && !merged.ipv6 && !merged.domain;
            setAddress(netName, empty ? null : merged);
          };
          return (
            <div key={netName} className="card" style={{ marginBottom: 8 }}>
              <strong>{netName}</strong> (public)
              <TextField id={`node-${name}-${netName}-ipv4`} label="IPv4" value={pub.ipv4 ?? ''} onChange={(v) => update({ ipv4: v || null })} disabled={disabled} />
              <TextField id={`node-${name}-${netName}-ipv6`} label="IPv6" value={pub.ipv6 ?? ''} onChange={(v) => update({ ipv6: v || null })} disabled={disabled} />
              <TextField id={`node-${name}-${netName}-domain`} label="Domain (CNAME)" value={pub.domain ?? ''} onChange={(v) => update({ domain: v || null })} disabled={disabled} />
            </div>
          );
        }
        const ip = typeof addr === 'string' ? addr : '';
        return (
          <IpField
            key={netName}
            id={`node-${name}-${netName}-ip`}
            label={`${netName} (${net.type})`}
            value={ip}
            onChange={(v) => setAddress(netName, v || null)}
            disabled={disabled}
          />
        );
      })}

      <h4>Drives</h4>
      <KeyValueList
        entries={drivePairs}
        onChange={(entries) => onChange({ drives: Object.fromEntries(entries.filter(([k]) => k)) })}
        keyLabel="Drive"
        valueLabel="Local path"
        disabled={disabled}
      />
    </div>
  );
}

function WebSection({ draft, setDraft, disabled }: SectionProps) {
  return (
    <fieldset>
      <legend>Web</legend>
      <TextField
        id="web-address"
        label="Address"
        value={draft.web.address}
        onChange={(address) => setDraft({ ...draft, web: { ...draft.web, address } })}
        hint="A bare label gets the network's domain appended."
        disabled={disabled}
      />
      <SelectField
        id="web-network"
        label="Network"
        value={draft.web.network}
        onChange={(network) => setDraft({ ...draft, web: { ...draft.web, network } })}
        options={Object.keys(draft.networks).map((n) => ({ value: n, label: n }))}
        disabled={disabled}
      />
      <PortField
        id="web-port"
        label="Port"
        value={draft.web.port}
        onChange={(port) => setDraft({ ...draft, web: { ...draft.web, port } })}
        disabled={disabled}
      />
    </fieldset>
  );
}

function FailoverSection({ draft, setDraft, disabled }: SectionProps) {
  const f = draft.failover;
  const set = (patch: Partial<ClusterConfig['failover']>) => setDraft({ ...draft, failover: { ...f, ...patch } });
  return (
    <fieldset>
      <legend>Failover</legend>
      <DurationField id="fo-node" label="Node timeout" value={f.node_timeout} onChange={(node_timeout) => set({ node_timeout })} disabled={disabled} />
      <DurationField id="fo-fence" label="Fence timeout" value={f.fence_timeout} onChange={(fence_timeout) => set({ fence_timeout })} disabled={disabled} hint="Must be shorter than the node timeout." />
      <DurationField id="fo-probe" label="Probe interval" value={f.probe_interval} onChange={(probe_interval) => set({ probe_interval })} disabled={disabled} />
    </fieldset>
  );
}

function SecretsSection({ draft, setDraft, disabled }: SectionProps) {
  const s = draft.secrets;
  const set = (patch: Partial<ClusterConfig['secrets']>) => setDraft({ ...draft, secrets: { ...s, ...patch } });
  return (
    <fieldset>
      <legend>Secrets</legend>
      <TextField id="sec-keyfile" label="Key file" value={s.key_file} onChange={(key_file) => set({ key_file })} disabled={disabled} />
      <CheckboxField id="sec-genmissing" label="Generate a key if missing" value={s.generate_if_missing} onChange={(generate_if_missing) => set({ generate_if_missing })} disabled={disabled} />
    </fieldset>
  );
}

function AuthSection({ draft, setDraft, disabled }: SectionProps) {
  const a = draft.auth;
  const set = (patch: Partial<ClusterConfig['auth']>) => setDraft({ ...draft, auth: { ...a, ...patch } });
  const hasOidc = a.oidc !== null;

  return (
    <fieldset>
      <legend>Auth</legend>
      <CheckboxField id="auth-apikeys" label="Allow API keys" value={a.api_keys} onChange={(api_keys) => set({ api_keys })} disabled={disabled} hint="Only turn off once OIDC is confirmed working." />
      <DurationField id="auth-ttl" label="Session TTL" value={a.session_ttl} onChange={(session_ttl) => set({ session_ttl })} disabled={disabled} />
      <CheckboxField
        id="auth-oidc-enabled"
        label="Use OIDC"
        value={hasOidc}
        onChange={(enabled) =>
          set({
            oidc: enabled
              ? { issuer: '', client_id: '', client_secret_file: null, scopes: ['openid', 'profile', 'email'], admin_claim: 'groups', admin_value: null }
              : null,
          })
        }
        disabled={disabled}
      />
      {a.oidc && (
        <div className="card" style={{ marginBottom: 8 }}>
          <TextField id="oidc-issuer" label="Issuer" value={a.oidc.issuer} onChange={(issuer) => set({ oidc: { ...a.oidc!, issuer } })} disabled={disabled} />
          <TextField id="oidc-clientid" label="Client ID" value={a.oidc.client_id} onChange={(client_id) => set({ oidc: { ...a.oidc!, client_id } })} disabled={disabled} />
          <TextField id="oidc-secretfile" label="Client secret file" value={a.oidc.client_secret_file ?? ''} onChange={(v) => set({ oidc: { ...a.oidc!, client_secret_file: v || null } })} disabled={disabled} />
          <TextField id="oidc-adminclaim" label="Admin claim" value={a.oidc.admin_claim} onChange={(admin_claim) => set({ oidc: { ...a.oidc!, admin_claim } })} disabled={disabled} />
          <TextField id="oidc-adminvalue" label="Admin value" value={a.oidc.admin_value ?? ''} onChange={(v) => set({ oidc: { ...a.oidc!, admin_value: v || null } })} disabled={disabled} />
        </div>
      )}
    </fieldset>
  );
}

function ProxySection({ draft, setDraft, disabled }: SectionProps) {
  const p = draft.proxy;
  const set = (patch: Partial<typeof p>) => setDraft({ ...draft, proxy: { ...p, ...patch } });
  return (
    <fieldset>
      <legend>Reverse proxy</legend>
      <CheckboxField
        id="proxy-enabled"
        label="Publish routes through the reverse proxy"
        value={p.enabled}
        onChange={(enabled) => set({ enabled })}
        disabled={disabled}
      />
      <PortField
        id="proxy-http-port"
        label="HTTP port"
        value={p.http_port}
        onChange={(http_port) => set({ http_port })}
        disabled={disabled || !p.enabled}
      />
      <PortField
        id="proxy-https-port"
        label="HTTPS port"
        value={p.https_port}
        onChange={(https_port) => set({ https_port })}
        hint="Must differ from the web ui's port, which listens on the same addresses."
        disabled={disabled || !p.enabled}
      />
      <CheckboxField
        id="proxy-acme-enabled"
        label="Issue certificates with ACME"
        value={p.acme.enabled}
        onChange={(enabled) => set({ acme: { ...p.acme, enabled } })}
        disabled={disabled || !p.enabled}
      />
      {p.acme.enabled && (
        <>
          <TextField
            id="proxy-acme-directory"
            label="Directory URL"
            value={p.acme.directory_url ?? ''}
            onChange={(v) => set({ acme: { ...p.acme, directory_url: v || null } })}
            hint="Leave blank for Let's Encrypt production."
            disabled={disabled}
          />
          <TextField
            id="proxy-acme-contact"
            label="Contact email"
            value={p.acme.contact_email ?? ''}
            onChange={(v) => set({ acme: { ...p.acme, contact_email: v || null } })}
            hint="Where the CA sends expiry warnings."
            disabled={disabled}
          />
        </>
      )}
    </fieldset>
  );
}

function CloudflareSection({ draft, setDraft, disabled }: SectionProps) {
  const cf = draft.cloudflare;
  return (
    <fieldset>
      <legend>Cloudflare</legend>
      <CheckboxField
        id="cf-enabled"
        label="Manage DNS through Cloudflare"
        value={cf !== null}
        onChange={(enabled) => setDraft({ ...draft, cloudflare: enabled ? { api_token_file: null, zones: [] } : null })}
        disabled={disabled}
      />
      {cf && (
        <>
          <TextField
            id="cf-tokenfile"
            label="API token file"
            value={cf.api_token_file ?? ''}
            onChange={(v) => setDraft({ ...draft, cloudflare: { ...cf, api_token_file: v || null } })}
            hint="Leave blank to set the token from the UI instead."
            disabled={disabled}
          />
          <TextField
            id="cf-zones"
            label="Zones (comma separated)"
            value={cf.zones.join(', ')}
            onChange={(v) =>
              setDraft({ ...draft, cloudflare: { ...cf, zones: v.split(',').map((z) => z.trim()).filter(Boolean) } })
            }
            disabled={disabled}
          />
        </>
      )}
    </fieldset>
  );
}
