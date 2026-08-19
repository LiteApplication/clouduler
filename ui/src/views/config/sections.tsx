// The individual configuration pages. Each one edits a slice of the draft
// config it is handed; none of them own state, so switching pages never
// loses an edit -- the draft lives in the Configuration shell above them.

import { useEffect, useState } from 'react';
import { api, ApiError } from '../../api/client';
import type {
  CloudflareTokenStatus,
  ClusterConfig,
  NetworkConfig,
  NetworkKind,
  NodeAddress,
  NodeConfig,
  PublicAddress,
} from '../../api/types';
import { Icon } from '../../components/Icon';
import {
  CheckboxField,
  CidrField,
  DurationField,
  IpField,
  KeyValueList,
  PortField,
  SelectField,
  TextField,
} from '../../components/FormFields';

export interface SectionProps {
  draft: ClusterConfig;
  setDraft: (c: ClusterConfig) => void;
  disabled: boolean;
}

export function ClusterSection({ draft, setDraft, disabled }: SectionProps) {
  return (
    <div className="card">
      <TextField
        id="cluster-name"
        label="Name"
        value={draft.cluster.name}
        onChange={(name) => setDraft({ ...draft, cluster: { ...draft.cluster, name } })}
        hint="Shown in the UI and used as the default suffix for internal DNS names."
        disabled={disabled}
      />
    </div>
  );
}

export function NetworksSection({ draft, setDraft, disabled }: SectionProps) {
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
    <>
      {entries.map(([name, net]) => (
        <div className="card section" key={name}>
          <div className="section-head">
            <h3 className="flex-row">
              <Icon name="network" /> {name}
            </h3>
            {!disabled && net.type !== 'internal' && (
              <button className="danger" onClick={() => removeNetwork(name)}>
                <Icon name="trash" /> Remove
              </button>
            )}
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
      {!disabled && (
        <button onClick={addNetwork}>
          <Icon name="plus" /> Add network
        </button>
      )}
    </>
  );
}

export function DrivesSection({ draft, setDraft, disabled }: SectionProps) {
  const entries = Object.entries(draft.drives);

  const update = (name: string, patch: Partial<ClusterConfig['drives'][string]>) => {
    setDraft({ ...draft, drives: { ...draft.drives, [name]: { ...draft.drives[name], ...patch } } });
  };

  const add = () => {
    let name = 'drive';
    let i = 1;
    while (draft.drives[name]) name = `drive${i++}`;
    setDraft({
      ...draft,
      drives: { ...draft.drives, [name]: { sync_interval: '15m', mode: 'replicated', served_by: null } },
    });
  };

  const remove = (name: string) => {
    const next = { ...draft.drives };
    delete next[name];
    setDraft({ ...draft, drives: next });
  };

  return (
    <>
      {entries.map(([name, drive]) => (
        <div className="card section" key={name}>
          <div className="section-head">
            <h3 className="flex-row">
              <Icon name="drives" /> {name}
            </h3>
            {!disabled && (
              <button className="danger" onClick={() => remove(name)}>
                <Icon name="trash" /> Remove
              </button>
            )}
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
              options={[
                { value: '', label: '(choose a node)' },
                ...Object.keys(draft.nodes).map((n) => ({ value: n, label: n })),
              ]}
              disabled={disabled}
            />
          )}
        </div>
      ))}
      {!disabled && (
        <button onClick={add}>
          <Icon name="plus" /> Add drive
        </button>
      )}
    </>
  );
}

export function NodesSection({ draft, setDraft, disabled }: SectionProps) {
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
    <>
      {entries.map(([name, node]) => (
        <NodeCard
          key={name}
          name={name}
          node={node}
          networks={draft.networks}
          onChange={(patch) => update(name, patch)}
          onRemove={() => remove(name)}
          disabled={disabled}
        />
      ))}
      {entries.length === 0 && <p className="muted">No nodes in the config yet.</p>}
    </>
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
      <div className="section-head">
        <h3 className="flex-row">
          <Icon name="nodes" /> {name}
        </h3>
        {!disabled && (
          <button className="danger" onClick={onRemove}>
            <Icon name="trash" /> Remove node
          </button>
        )}
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
        value={
          node.generate_secret_key === null || node.generate_secret_key === undefined
            ? 'inherit'
            : String(node.generate_secret_key)
        }
        onChange={(choice) => onChange({ generate_secret_key: choice === 'inherit' ? null : choice === 'true' })}
        options={[
          { value: 'inherit', label: 'Follow the cluster setting' },
          { value: 'true', label: 'Yes, this node may create it' },
          { value: 'false', label: 'Never, it can only be given the key' },
        ]}
        disabled={disabled}
      />

      <h4 className="flex-row">
        <Icon name="network" /> Network addresses
      </h4>
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
              <strong>{netName}</strong> <span className="faint">(public)</span>
              <TextField
                id={`node-${name}-${netName}-ipv4`}
                label="IPv4"
                value={pub.ipv4 ?? ''}
                onChange={(v) => update({ ipv4: v || null })}
                disabled={disabled}
              />
              <TextField
                id={`node-${name}-${netName}-ipv6`}
                label="IPv6"
                value={pub.ipv6 ?? ''}
                onChange={(v) => update({ ipv6: v || null })}
                disabled={disabled}
              />
              <TextField
                id={`node-${name}-${netName}-domain`}
                label="Domain (CNAME)"
                value={pub.domain ?? ''}
                onChange={(v) => update({ domain: v || null })}
                disabled={disabled}
              />
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

      <h4 className="flex-row">
        <Icon name="drives" /> Drives
      </h4>
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

export function WebSection({ draft, setDraft, disabled }: SectionProps) {
  return (
    <div className="card">
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
        label="Direct HTTPS port"
        value={draft.web.port}
        onChange={(port) => setDraft({ ...draft, web: { ...draft.web, port } })}
        hint="Always listening, even when the dashboard is also published through the proxy. This is the way back in if the proxy cannot bind its ports."
        disabled={disabled}
      />
      <CheckboxField
        id="web-through-proxy"
        label="Publish the dashboard through the reverse proxy"
        value={draft.web.through_proxy}
        onChange={(through_proxy) => setDraft({ ...draft, web: { ...draft.web, through_proxy } })}
        hint="Serves it on the standard HTTPS port with the same Let's Encrypt certificate and Cloudflare record as any other route."
        disabled={disabled || !draft.proxy.enabled}
      />
      {draft.web.through_proxy && (
        <PortField
          id="web-backend-port"
          label="Proxy backend port"
          value={draft.web.backend_port}
          onChange={(backend_port) => setDraft({ ...draft, web: { ...draft.web, backend_port } })}
          hint="Loopback only, plain HTTP, reachable only by this node's proxy. Must differ from every other port here."
          disabled={disabled}
        />
      )}
    </div>
  );
}

export function FailoverSection({ draft, setDraft, disabled }: SectionProps) {
  const f = draft.failover;
  const set = (patch: Partial<ClusterConfig['failover']>) => setDraft({ ...draft, failover: { ...f, ...patch } });
  return (
    <div className="card">
      <DurationField
        id="fo-node"
        label="Node timeout"
        value={f.node_timeout}
        onChange={(node_timeout) => set({ node_timeout })}
        disabled={disabled}
      />
      <DurationField
        id="fo-fence"
        label="Fence timeout"
        value={f.fence_timeout}
        onChange={(fence_timeout) => set({ fence_timeout })}
        disabled={disabled}
        hint="Must be shorter than the node timeout."
      />
      <DurationField
        id="fo-probe"
        label="Probe interval"
        value={f.probe_interval}
        onChange={(probe_interval) => set({ probe_interval })}
        disabled={disabled}
      />
    </div>
  );
}

// The key file path is deliberately not editable here. Repointing it from the
// ui is never what someone means: the key it names is what opens every stored
// secret, and a wrong path silently produces a node that replicates secrets it
// cannot decrypt. Moving it is a deliberate, on-the-box operation. The value
// still rides along in the draft, so it survives an edit made from this page.
export function SecretsSection({ draft, setDraft, disabled }: SectionProps) {
  const s = draft.secrets;
  const set = (patch: Partial<ClusterConfig['secrets']>) => setDraft({ ...draft, secrets: { ...s, ...patch } });
  return (
    <div className="card">
      <CheckboxField
        id="sec-genmissing"
        label="Generate a key if missing"
        value={s.generate_if_missing}
        onChange={(generate_if_missing) => set({ generate_if_missing })}
        disabled={disabled}
      />
    </div>
  );
}

export function AuthSection({ draft, setDraft, disabled }: SectionProps) {
  const a = draft.auth;
  const set = (patch: Partial<ClusterConfig['auth']>) => setDraft({ ...draft, auth: { ...a, ...patch } });
  const hasOidc = a.oidc != null;

  return (
    <div className="card">
      <CheckboxField
        id="auth-apikeys"
        label="Allow API keys"
        value={a.api_keys}
        onChange={(api_keys) => set({ api_keys })}
        disabled={disabled}
        hint="Only turn off once OIDC is confirmed working."
      />
      <DurationField
        id="auth-ttl"
        label="Session TTL"
        value={a.session_ttl}
        onChange={(session_ttl) => set({ session_ttl })}
        disabled={disabled}
      />
      <CheckboxField
        id="auth-oidc-enabled"
        label="Use OIDC"
        value={hasOidc}
        onChange={(enabled) =>
          set({
            oidc: enabled
              ? {
                  issuer: '',
                  client_id: '',
                  client_secret_file: null,
                  scopes: ['openid', 'profile', 'email'],
                  admin_claim: 'groups',
                  admin_value: null,
                }
              : null,
          })
        }
        disabled={disabled}
      />
      {a.oidc && (
        <div className="card" style={{ marginTop: 8 }}>
          <TextField
            id="oidc-issuer"
            label="Issuer"
            value={a.oidc.issuer}
            onChange={(issuer) => set({ oidc: { ...a.oidc!, issuer } })}
            disabled={disabled}
          />
          <TextField
            id="oidc-clientid"
            label="Client ID"
            value={a.oidc.client_id}
            onChange={(client_id) => set({ oidc: { ...a.oidc!, client_id } })}
            disabled={disabled}
          />
          <TextField
            id="oidc-secretfile"
            label="Client secret file"
            value={a.oidc.client_secret_file ?? ''}
            onChange={(v) => set({ oidc: { ...a.oidc!, client_secret_file: v || null } })}
            disabled={disabled}
          />
          <TextField
            id="oidc-adminclaim"
            label="Admin claim"
            value={a.oidc.admin_claim}
            onChange={(admin_claim) => set({ oidc: { ...a.oidc!, admin_claim } })}
            disabled={disabled}
          />
          <TextField
            id="oidc-adminvalue"
            label="Admin value"
            value={a.oidc.admin_value ?? ''}
            onChange={(v) => set({ oidc: { ...a.oidc!, admin_value: v || null } })}
            disabled={disabled}
          />
        </div>
      )}
    </div>
  );
}

export function ProxySection({ draft, setDraft, disabled }: SectionProps) {
  const p = draft.proxy;
  const set = (patch: Partial<typeof p>) => setDraft({ ...draft, proxy: { ...p, ...patch } });
  return (
    <div className="card">
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
      <p className="field-hint">
        Static/external backends and redirects apply immediately from the Redirects page, since
        they live outside cluster.yaml. See docs/proxy.md.
      </p>
    </div>
  );
}

export function CloudflareSection({ draft, setDraft, disabled }: SectionProps) {
  const cf = draft.cloudflare;
  return (
    <div className="card">
      <CheckboxField
        id="cf-enabled"
        label="Manage DNS through Cloudflare"
        value={cf != null}
        onChange={(enabled) => setDraft({ ...draft, cloudflare: enabled ? { api_token_file: null, zones: [] } : null })}
        disabled={disabled}
      />
      {cf && (
        <>
          <CloudflareToken disabled={disabled} />
          <TextField
            id="cf-zones"
            label="Zones (comma separated)"
            value={cf.zones.join(', ')}
            onChange={(v) =>
              setDraft({
                ...draft,
                cloudflare: { ...cf, zones: v.split(',').map((z) => z.trim()).filter(Boolean) },
              })
            }
            disabled={disabled}
          />
        </>
      )}
    </div>
  );
}

/**
 * The API token, set here rather than by naming a file the operator has to
 * create over ssh first.
 *
 * It sits outside the config draft on purpose. The token is written by its own
 * endpoint the moment Save is pressed, not on Apply, because it is not a config
 * value that can be diffed, rolled back or shown: once stored it is never
 * readable again, so this box disappears as soon as one is set and rotating
 * means replacing the file on disk.
 */
function CloudflareToken({ disabled }: { disabled: boolean }) {
  const [status, setStatus] = useState<CloudflareTokenStatus | null>(null);
  const [token, setToken] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.cloudflareToken().then(setStatus).catch(() => setStatus(null));
  }, []);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.setCloudflareToken(token.trim());
      setToken('');
      setStatus(await api.cloudflareToken());
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'could not save the token');
    } finally {
      setBusy(false);
    }
  };

  if (!status) {
    return (
      <p className="loading">
        <span className="spinner" /> Checking the API token...
      </p>
    );
  }

  if (status.configured) {
    return (
      <div className="field">
        <label>API token</label>
        <p className="muted">
          Stored in <span className="mono">{status.path}</span>. It cannot be shown again; replace that file to
          rotate it.
        </p>
        {!status.present_on_this_node && (
          <span className="field-error">
            <Icon name="alert-circle" />
            That file is missing on this node, so DNS challenges from here will fail. Copy it across.
          </span>
        )}
      </div>
    );
  }

  return (
    <div className="field">
      <label htmlFor="cf-token">API token</label>
      <input
        id="cf-token"
        type="password"
        value={token}
        onChange={(e) => setToken(e.target.value)}
        placeholder="Cloudflare API token"
        autoComplete="off"
        disabled={disabled || busy}
      />
      <span className="field-hint">
        Saved to the config directory as soon as you press Save, separately from the rest of this page. You will not
        be able to read it back.
      </span>
      {error && (
        <span className="field-error">
          <Icon name="alert-circle" /> {error}
        </span>
      )}
      <div className="btn-row" style={{ marginTop: 8 }}>
        <button onClick={save} disabled={disabled || busy || token.trim().length === 0}>
          {busy ? <span className="spinner" /> : <Icon name="key" />}
          {busy ? 'Saving...' : 'Save token'}
        </button>
      </div>
    </div>
  );
}
