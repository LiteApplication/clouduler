// The individual configuration pages. Each one edits a slice of the draft
// config it is handed; none of them own state, so switching pages never
// loses an edit because the draft lives in the Configuration shell above them.

import { generateX25519Identity, identityToRecipient } from 'age-encryption';
import { useEffect, useState } from 'react';
import { api, ApiError, encodeText } from '../../api/client';
import type {
  CloudflareTokenStatus,
  ClusterConfig,
  EgressFallback,
  GitBackupStatus,
  InterfaceAddress,
  NetworkConfig,
  NetworkKind,
  NodeAddress,
  NodeConfig,
  OidcClientSecretStatus,
  OpenPort,
  PortProtocol,
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
  /** The config as last fetched from the cluster, before any staged edits.
   *  Lets a section tell an already-applied network from one that only
   *  exists in the unsaved draft (e.g. just added by "Add network"), which
   *  a rename against the live `/api/config/networks/{name}/rename` cannot
   *  know about yet. */
  original: ClusterConfig;
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

/** Applies the same key rename the server does, to every place a network
 *  name appears in a `ClusterConfig`, so the draft stays in sync with a
 *  `renameNetwork` call without wiping any other unrelated staged edit. */
function renameNetworkInDraft(draft: ClusterConfig, oldName: string, newName: string): ClusterConfig {
  const networks = { ...draft.networks };
  const net = networks[oldName];
  delete networks[oldName];
  networks[newName] = net;

  const nodes = Object.fromEntries(
    Object.entries(draft.nodes).map(([nodeName, node]) => {
      if (!(oldName in node.network)) return [nodeName, node];
      const network = { ...node.network };
      const addr = network[oldName];
      delete network[oldName];
      network[newName] = addr;
      return [nodeName, { ...node, network }];
    }),
  );

  return {
    ...draft,
    networks,
    nodes,
    web: draft.web.network === oldName ? { ...draft.web, network: newName } : draft.web,
  };
}

export function NetworksSection({ draft, setDraft, disabled, original }: SectionProps) {
  const entries = Object.entries(draft.networks);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState('');
  const [renameError, setRenameError] = useState<string | null>(null);
  const [renameBusy, setRenameBusy] = useState(false);

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

  const startRename = (name: string) => {
    setRenaming(name);
    setRenameValue(name);
    setRenameError(null);
  };

  const submitRename = async (oldName: string) => {
    const newName = renameValue.trim();
    if (!newName || newName === oldName) {
      setRenaming(null);
      return;
    }
    if (draft.networks[newName]) {
      setRenameError(`a network named ${newName} already exists`);
      return;
    }
    // A network only "Add network" staged, never applied, exists nowhere the
    // server knows to rename: renaming it is just a local key swap on the
    // draft, the same as `addNetwork` picking the name in the first place.
    if (!(oldName in original.networks)) {
      setDraft(renameNetworkInDraft(draft, oldName, newName));
      setRenaming(null);
      return;
    }
    setRenameBusy(true);
    setRenameError(null);
    try {
      await api.renameNetwork(oldName, newName);
      setDraft(renameNetworkInDraft(draft, oldName, newName));
      setRenaming(null);
    } catch (e) {
      setRenameError(e instanceof ApiError ? e.message : 'rename failed');
    } finally {
      setRenameBusy(false);
    }
  };

  return (
    <>
      {entries.map(([name, net]) => (
        <div className="card section" key={name}>
          <div className="section-head">
            {renaming === name ? (
              <div className="flex-row" style={{ gap: 8 }}>
                <input
                  className="mono"
                  value={renameValue}
                  onChange={(e) => setRenameValue(e.target.value)}
                  disabled={renameBusy}
                  aria-label={`rename network ${name}`}
                />
                <button onClick={() => submitRename(name)} disabled={renameBusy}>
                  <Icon name="check" /> Save
                </button>
                <button onClick={() => setRenaming(null)} disabled={renameBusy}>
                  Cancel
                </button>
              </div>
            ) : (
              <h3 className="flex-row">
                <Icon name="network" /> {name}
                {!disabled && (
                  <button className="ghost" onClick={() => startRename(name)} aria-label={`rename ${name}`}>
                    <Icon name="edit" />
                  </button>
                )}
              </h3>
            )}
            {!disabled && net.type !== 'internal' && (
              <button className="danger" onClick={() => removeNetwork(name)}>
                <Icon name="trash" /> Remove
              </button>
            )}
          </div>
          {renaming === name && renameError && (
            <p className="field-error">
              <Icon name="alert-circle" /> {renameError}
            </p>
          )}
          <SelectField
            id={`net-${name}-type`}
            label="Type"
            value={net.type}
            onChange={(type) => updateNetwork(name, { type: type as NetworkKind })}
            options={[
              { value: 'internal', label: 'Internal' },
              { value: 'private', label: 'Private' },
              { value: 'public', label: 'Public' },
              { value: 'egress', label: 'Egress' },
              { value: 'wireguard', label: 'WireGuard' },
              { value: 'nat', label: 'NAT' },
            ]}
            hint="Internal carries raft, drive sync and the container overlay, and is required exactly once. Private is a subnetted network for other trusted traffic. Public has no subnet; nodes get an IPv4/IPv6/domain address block instead. Egress routes a service's outbound traffic out a named host interface instead of the node's normal route; pick that interface per node below. WireGuard is configured per node by importing a wg-quick config below. NAT routes a service's outbound traffic through the node's own routing table, masqueraded like ordinary NAT, instead of a named interface — no per-node setup needed."
            disabled={disabled}
          />
          {net.type !== 'public' && net.type !== 'wireguard' && net.type !== 'egress' && net.type !== 'nat' && (
            <CidrField
              id={`net-${name}-subnet`}
              label="Subnet"
              value={net.subnet ?? ''}
              onChange={(subnet) => updateNetwork(name, { subnet: subnet || null })}
              hint="The address range nodes on this network get their addresses from, e.g. 10.10.0.0/24."
              disabled={disabled}
            />
          )}
          {net.type !== 'wireguard' && net.type !== 'egress' && net.type !== 'nat' && (
            <TextField
              id={`net-${name}-domain`}
              label="Domain"
              value={net.domain ?? ''}
              onChange={(domain) => updateNetwork(name, { domain: domain || null })}
              hint="Records under this domain resolve to node addresses on this network."
              disabled={disabled}
            />
          )}
          {net.type === 'wireguard' && (
            <p className="field-hint">
              Not addressable: import each node's WireGuard config below, under its own card.
            </p>
          )}
          {net.type === 'egress' && (
            <p className="field-hint">
              Not addressable by subnet: pick a host interface per node below. Clouduler does not
              manage the interface itself — it must already exist and be up.
            </p>
          )}
          {net.type === 'nat' && (
            <p className="field-hint">
              Not addressable, and nothing to configure per node: every node implements it the
              same way, NAT-ing a service's outbound traffic out whatever interface its own
              routing table picks, instead of a single fixed interface.
            </p>
          )}
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

/** Applies the same key rename the server does, to every place a drive name
 *  appears in a `ClusterConfig`, so the draft stays in sync with a
 *  `renameDrive` call without wiping any other unrelated staged edit. */
function renameDriveInDraft(draft: ClusterConfig, oldName: string, newName: string): ClusterConfig {
  const drives = { ...draft.drives };
  const drive = drives[oldName];
  delete drives[oldName];
  drives[newName] = drive;

  const nodes = Object.fromEntries(
    Object.entries(draft.nodes).map(([nodeName, node]) => {
      if (!(oldName in node.drives)) return [nodeName, node];
      const nodeDrives = { ...node.drives };
      const path = nodeDrives[oldName];
      delete nodeDrives[oldName];
      nodeDrives[newName] = path;
      return [nodeName, { ...node, drives: nodeDrives }];
    }),
  );

  return { ...draft, drives, nodes };
}

export function DrivesSection({ draft, setDraft, disabled, original }: SectionProps) {
  const entries = Object.entries(draft.drives);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState('');
  const [renameError, setRenameError] = useState<string | null>(null);
  const [renameBusy, setRenameBusy] = useState(false);

  const update = (name: string, patch: Partial<ClusterConfig['drives'][string]>) => {
    setDraft({ ...draft, drives: { ...draft.drives, [name]: { ...draft.drives[name], ...patch } } });
  };

  const add = () => {
    let name = 'drive';
    let i = 1;
    while (draft.drives[name]) name = `drive${i++}`;
    setDraft({
      ...draft,
      drives: { ...draft.drives, [name]: { sync_interval: '15m', mode: 'replicated', served_by: [] } },
    });
  };

  const remove = (name: string) => {
    const next = { ...draft.drives };
    delete next[name];
    setDraft({ ...draft, drives: next });
  };

  const startRename = (name: string) => {
    setRenaming(name);
    setRenameValue(name);
    setRenameError(null);
  };

  const submitRename = async (oldName: string) => {
    const newName = renameValue.trim();
    if (!newName || newName === oldName) {
      setRenaming(null);
      return;
    }
    if (draft.drives[newName]) {
      setRenameError(`a drive named ${newName} already exists`);
      return;
    }
    // A drive only "Add drive" staged, never applied, exists nowhere the
    // server knows to rename: renaming it is just a local key swap on the
    // draft, the same as `add` picking the name in the first place.
    if (!(oldName in original.drives)) {
      setDraft(renameDriveInDraft(draft, oldName, newName));
      setRenaming(null);
      return;
    }
    setRenameBusy(true);
    setRenameError(null);
    try {
      await api.renameDrive(oldName, newName);
      setDraft(renameDriveInDraft(draft, oldName, newName));
      setRenaming(null);
    } catch (e) {
      setRenameError(e instanceof ApiError ? e.message : 'rename failed');
    } finally {
      setRenameBusy(false);
    }
  };

  return (
    <>
      {entries.map(([name, drive]) => (
        <div className="card section" key={name}>
          <div className="section-head">
            {renaming === name ? (
              <div className="flex-row" style={{ gap: 8 }}>
                <input
                  className="mono"
                  value={renameValue}
                  onChange={(e) => setRenameValue(e.target.value)}
                  disabled={renameBusy}
                  aria-label={`rename drive ${name}`}
                />
                <button onClick={() => submitRename(name)} disabled={renameBusy}>
                  <Icon name="check" /> Save
                </button>
                <button onClick={() => setRenaming(null)} disabled={renameBusy}>
                  Cancel
                </button>
              </div>
            ) : (
              <h3 className="flex-row">
                <Icon name="drives" /> {name}
                {!disabled && (
                  <button className="ghost" onClick={() => startRename(name)} aria-label={`rename ${name}`}>
                    <Icon name="edit" />
                  </button>
                )}
              </h3>
            )}
            {!disabled && (
              <button className="danger" onClick={() => remove(name)}>
                <Icon name="trash" /> Remove
              </button>
            )}
          </div>
          {renaming === name && renameError && (
            <p className="field-error">
              <Icon name="alert-circle" /> {renameError}
            </p>
          )}
          <SelectField
            id={`drive-${name}-mode`}
            label="Mode"
            value={drive.mode}
            onChange={(mode) => update(name, { mode: mode as 'replicated' | 'served' })}
            options={[
              { value: 'replicated', label: 'Replicated' },
              { value: 'served', label: 'Served' },
            ]}
            hint="Replicated keeps a synced copy on every node that implements this drive, resynced on the interval below. Served keeps one active copy on a single node, with the rest as standby backups (pick the candidate order below)."
            disabled={disabled}
          />
          {drive.mode === 'replicated' ? (
            <DurationField
              id={`drive-${name}-interval`}
              label="Sync interval"
              value={drive.sync_interval}
              onChange={(sync_interval) => update(name, { sync_interval })}
              hint="How often each replica resyncs against the primary copy, e.g. 15m. A stack can override this per volume."
              disabled={disabled}
            />
          ) : (
            <ServedByField
              id={`drive-${name}-servedby`}
              servedBy={drive.served_by}
              nodeNames={Object.keys(draft.nodes)}
              onChange={(served_by) => update(name, { served_by })}
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

/** Picks the ordered list of candidate serving nodes for a served drive. The
 *  first checked node, in the order checked, is the active server; the rest
 *  are standby backups in priority order. Reorder with the arrows. */
function ServedByField({
  id,
  servedBy,
  nodeNames,
  onChange,
  disabled,
}: {
  id: string;
  servedBy: string[];
  nodeNames: string[];
  onChange: (servedBy: string[]) => void;
  disabled?: boolean;
}) {
  const toggle = (node: string) => {
    if (servedBy.includes(node)) onChange(servedBy.filter((n) => n !== node));
    else onChange([...servedBy, node]);
  };
  const move = (index: number, delta: number) => {
    const target = index + delta;
    if (target < 0 || target >= servedBy.length) return;
    const next = [...servedBy];
    [next[index], next[target]] = [next[target], next[index]];
    onChange(next);
  };

  return (
    <div className="field">
      <label id={`${id}-label`}>Served by</label>
      <span className="field-hint">
        The active server is the first reachable candidate; the rest keep a synced backup copy.
      </span>
      {nodeNames.map((node) => (
        <label className="flex-row" key={node} htmlFor={`${id}-${node}`}>
          <input
            id={`${id}-${node}`}
            type="checkbox"
            checked={servedBy.includes(node)}
            onChange={() => toggle(node)}
            disabled={disabled}
          />
          {node}
        </label>
      ))}
      {servedBy.length > 0 && (
        <ol style={{ marginTop: 8 }}>
          {servedBy.map((node, i) => (
            <li key={node} className="flex-row">
              {node}
              {!disabled && (
                <>
                  <button
                    type="button"
                    className="ghost"
                    onClick={() => move(i, -1)}
                    disabled={i === 0}
                    aria-label={`move ${node} up`}
                  >
                    &#9650;
                  </button>
                  <button
                    type="button"
                    className="ghost"
                    onClick={() => move(i, 1)}
                    disabled={i === servedBy.length - 1}
                    aria-label={`move ${node} down`}
                  >
                    &#9660;
                  </button>
                </>
              )}
            </li>
          ))}
        </ol>
      )}
    </div>
  );
}

/** A node's config-level card: role, ports, addresses and drive mounts. Used
 *  standalone on a node's detail page (see `NodeDetail`), scoped to just that
 *  one node rather than listed alongside every other node. */
export function NodeCard({
  name,
  node,
  networks,
  drives,
  onChange,
  onRemove,
  disabled,
}: {
  name: string;
  node: NodeConfig;
  networks: ClusterConfig['networks'];
  drives: ClusterConfig['drives'];
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

  // Internal is mandatory for every node, so it's always shown. Every other
  // kind is opt-in per node: only render its editor once explicitly added,
  // and offer the rest through "Add network" below.
  const networkEntries = Object.entries(networks);
  // A wireguard network has no value to write into the draft until its
  // config is imported (the interface name comes back from that call), so
  // "adding" one just reveals its import form (tracked here rather than
  // in node.network) until the operator actually imports something.
  const [pendingWireguard, setPendingWireguard] = useState<string[]>([]);
  const joinableNetworks = networkEntries.filter(
    ([netName, net]) =>
      net.type !== 'internal' &&
      net.type !== 'nat' &&
      !(netName in node.network) &&
      !pendingWireguard.includes(netName),
  );
  const [addingNetwork, setAddingNetwork] = useState('');

  const addNetworkDefault = (netName: string) => {
    const net = networks[netName];
    if (!net) return;
    if (net.type === 'wireguard') {
      setPendingWireguard((prev) => (prev.includes(netName) ? prev : [...prev, netName]));
      return;
    }
    const value: NodeAddress = net.type === 'public' ? { ipv4: null, ipv6: null, domain: null, open_ports: [] } : '';
    setAddress(netName, value);
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
        hint="Managers take part in raft consensus and can become leader; workers only run stacks. A cluster needs at least one manager, and an odd number of managers to keep quorum decisive."
        disabled={disabled}
      />
      <PortField
        id={`node-${name}-port`}
        label="RPC port"
        value={node.rpc_port}
        onChange={(rpc_port) => onChange({ rpc_port })}
        hint="The port other nodes dial, on this node's internal-network address, to reach its cluster RPC service."
        disabled={disabled}
      />
      <TextField
        id={`node-${name}-datadir`}
        label="Data dir override"
        value={node.data_dir ?? ''}
        onChange={(data_dir) => onChange({ data_dir: data_dir || null })}
        hint="Overrides the cluster-wide data directory for this node only. Leave blank to use the cluster default."
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
        hint="Only the first node bootstrapping a brand new cluster should be allowed to generate the key. A node joining an existing cluster should be set to 'Never', since a second key could not open secrets the rest of the cluster already sealed."
        disabled={disabled}
      />

      <h4 className="flex-row">
        <Icon name="network" /> Network addresses
      </h4>
      {networkEntries.map(([netName, net]) => {
        const addr = node.network[netName];
        if (net.type === 'nat') {
          // Not addressable per node: nothing to render here at all.
          return null;
        } else if (net.type === 'internal') {
          // always shown, falls through below
        } else if (net.type === 'wireguard') {
          if (!(netName in node.network) && !pendingWireguard.includes(netName)) return null;
        } else if (!(netName in node.network)) {
          return null;
        }
        if (net.type === 'wireguard') {
          const wg: InterfaceAddress | null =
            typeof addr === 'string'
              ? { interface: addr, open_ports: [] }
              : addr && 'interface' in addr
                ? { interface: addr.interface, open_ports: addr.open_ports ?? [] }
                : null;
          return (
            <WireguardAddressField
              key={netName}
              nodeName={name}
              netName={netName}
              addr={wg}
              onChange={(v) => setAddress(netName, v ? { interface: v.interface, open_ports: v.open_ports } : null)}
              onCancel={
                wg === null ? () => setPendingWireguard((prev) => prev.filter((n) => n !== netName)) : undefined
              }
              disabled={disabled}
            />
          );
        }
        if (net.type === 'public') {
          const pub = (typeof addr === 'object' ? addr : {}) as Partial<PublicAddress>;
          const update = (patch: Partial<PublicAddress>) => {
            const merged: PublicAddress = {
              ipv4: pub.ipv4 ?? null,
              ipv6: pub.ipv6 ?? null,
              domain: pub.domain ?? null,
              open_ports: pub.open_ports ?? [],
              ...patch,
            };
            const empty = !merged.ipv4 && !merged.ipv6 && !merged.domain && merged.open_ports.length === 0;
            setAddress(netName, empty ? null : merged);
          };
          return (
            <div key={netName} className="card" style={{ marginBottom: 8 }}>
              <div className="section-head">
                <strong>{netName}</strong> <span className="faint">(public)</span>
                {!disabled && (
                  <button
                    className="ghost"
                    onClick={() => setAddress(netName, null)}
                    aria-label={`remove ${netName} from ${name}`}
                    title={`Remove ${netName} from this node`}
                  >
                    <Icon name="close" />
                  </button>
                )}
              </div>
              <TextField
                id={`node-${name}-${netName}-ipv4`}
                label="IPv4"
                value={pub.ipv4 ?? ''}
                onChange={(v) => update({ ipv4: v || null })}
                hint="This node's public IPv4 address on this network. Publishes an A record if the network has a domain."
                disabled={disabled}
              />
              <TextField
                id={`node-${name}-${netName}-ipv6`}
                label="IPv6"
                value={pub.ipv6 ?? ''}
                onChange={(v) => update({ ipv6: v || null })}
                hint="This node's public IPv6 address on this network. Publishes an AAAA record if the network has a domain."
                disabled={disabled}
              />
              <TextField
                id={`node-${name}-${netName}-domain`}
                label="Domain (CNAME)"
                value={pub.domain ?? ''}
                onChange={(v) => update({ domain: v || null })}
                hint="Points this node's DNS record at another hostname instead of a fixed IP, e.g. for a dynamic-DNS address."
                disabled={disabled}
              />
              <OpenPortsEditor
                idPrefix={`node-${name}-${netName}`}
                hint="Ports this node's router forwards to it here. A raw port a stack exposes on this network only ever schedules onto a node that lists it."
                ports={pub.open_ports ?? []}
                onChange={(open_ports) => update({ open_ports })}
                disabled={disabled}
              />
            </div>
          );
        }
        if (net.type === 'egress') {
          const egress: InterfaceAddress =
            typeof addr === 'string'
              ? { interface: addr, open_ports: [], fallback: null }
              : addr && 'interface' in addr
                ? { interface: addr.interface, open_ports: addr.open_ports ?? [], fallback: addr.fallback ?? null }
                : { interface: '', open_ports: [], fallback: null };
          return (
            <EgressInterfaceField
              key={netName}
              nodeName={name}
              netName={netName}
              addr={egress}
              onChange={(v) => setAddress(netName, v.interface ? v : null)}
              disabled={disabled}
            />
          );
        }
        const ip = typeof addr === 'string' ? addr : '';
        return (
          <div key={netName} className="flex-row" style={{ gap: 8, alignItems: 'flex-end' }}>
            <div style={{ flex: 1 }}>
              <IpField
                id={`node-${name}-${netName}-ip`}
                label={`${netName} (${net.type})`}
                value={ip}
                onChange={(v) => setAddress(netName, v || null)}
                hint={
                  net.type === 'internal'
                    ? "This node's fixed address on the internal network, inside its subnet. Required for every node because raft, drive sync and the container overlay all use it."
                    : "This node's fixed address on this network, inside its subnet."
                }
                disabled={disabled}
              />
            </div>
            {!disabled && net.type !== 'internal' && (
              <button
                className="ghost"
                onClick={() => setAddress(netName, null)}
                aria-label={`remove ${netName} from ${name}`}
                title={`Remove ${netName} from this node`}
              >
                <Icon name="close" />
              </button>
            )}
          </div>
        );
      })}

      {!disabled && joinableNetworks.length > 0 && (
        <div className="flex-row" style={{ gap: 8, alignItems: 'flex-end' }}>
          <div style={{ flex: 1 }}>
            <SelectField
              id={`node-${name}-add-network`}
              label="Add network"
              value={addingNetwork}
              onChange={setAddingNetwork}
              options={[
                { value: '', label: '(choose a network)' },
                ...joinableNetworks.map(([netName]) => ({ value: netName, label: netName })),
              ]}
              disabled={disabled}
            />
          </div>
          <button
            onClick={() => {
              if (!addingNetwork) return;
              addNetworkDefault(addingNetwork);
              setAddingNetwork('');
            }}
            disabled={!addingNetwork}
          >
            <Icon name="plus" /> Add
          </button>
        </div>
      )}

      <h4 className="flex-row">
        <Icon name="drives" /> Drives
      </h4>
      <KeyValueList
        entries={drivePairs}
        onChange={(entries) => onChange({ drives: Object.fromEntries(entries.filter(([k]) => k)) })}
        keyLabel="Drive"
        valueLabel="Local path"
        keyOptions={Object.keys(drives)}
        disabled={disabled}
      />
    </div>
  );
}

/** Ports a node has open on one network's address: a router forwarding them
 *  in (public) or a VPN tunnel's far end forwarding them back (wireguard). */
function OpenPortsEditor({
  idPrefix,
  hint,
  ports,
  onChange,
  disabled,
}: {
  idPrefix: string;
  hint: string;
  ports: OpenPort[];
  onChange: (ports: OpenPort[]) => void;
  disabled: boolean;
}) {
  const [adding, setAdding] = useState(false);
  const [port, setPort] = useState(0);
  const [protocol, setProtocol] = useState<PortProtocol>('tcp');

  const add = () => {
    if (!port || ports.some((p) => p.port === port && p.protocol === protocol)) return;
    onChange([...ports, { port, protocol }].sort((a, b) => a.port - b.port || a.protocol.localeCompare(b.protocol)));
    setPort(0);
    setAdding(false);
  };

  const remove = (target: OpenPort) => {
    onChange(ports.filter((p) => !(p.port === target.port && p.protocol === target.protocol)));
  };

  return (
    <div className="field">
      <label>Open ports</label>
      <p className="field-hint">{hint}</p>
      {ports.map((p) => (
        <div key={`${p.protocol}-${p.port}`} className="flex-row" style={{ gap: 8, marginBottom: 4 }}>
          <span className="mono">
            {p.port}/{p.protocol}
          </span>
          {!disabled && (
            <button type="button" onClick={() => remove(p)} aria-label={`remove ${p.port}/${p.protocol}`}>
              <Icon name="trash" />
            </button>
          )}
        </div>
      ))}
      {!disabled && !adding && (
        <button type="button" onClick={() => setAdding(true)}>
          <Icon name="plus" /> Add port
        </button>
      )}
      {!disabled && adding && (
        <div className="flex-row" style={{ gap: 8 }}>
          <PortField id={`${idPrefix}-open-port`} label="Port" value={port} onChange={setPort} disabled={disabled} />
          <SelectField
            id={`${idPrefix}-open-port-protocol`}
            label="Protocol"
            value={protocol}
            onChange={setProtocol}
            options={[
              { value: 'tcp', label: 'TCP' },
              { value: 'udp', label: 'UDP' },
            ]}
            disabled={disabled}
          />
          <button type="button" onClick={add} disabled={!port}>
            <Icon name="check" /> Add
          </button>
          <button type="button" onClick={() => setAdding(false)}>
            Cancel
          </button>
        </div>
      )}
    </div>
  );
}

/**
 * An egress-kind network's node address is a host interface name (`wg0`,
 * `end0`, ...) rather than an IP: `Docker::set_default_route` forces the
 * service's outbound traffic out that interface. Suggests the node's current
 * non-docker, non-clouduler-managed interfaces via a datalist, fetched once
 * per mount, but stays free text since the interface may not exist yet, or
 * the node may be unreachable right now.
 *
 * Optionally also names a fallback interface: every reconcile tick, the
 * daemon pings `fallback.health_check_ip` out the primary interface, and
 * routes the network out the fallback instead for as long as that fails —
 * switching back automatically once the primary answers again.
 */
function EgressInterfaceField({
  nodeName,
  netName,
  addr,
  onChange,
  disabled,
}: {
  nodeName: string;
  netName: string;
  addr: InterfaceAddress;
  onChange: (addr: InterfaceAddress) => void;
  disabled: boolean;
}) {
  const listId = `node-${nodeName}-${netName}-iface-options`;
  const [options, setOptions] = useState<string[]>([]);
  const [fallbackEnabled, setFallbackEnabled] = useState(!!addr.fallback);

  useEffect(() => {
    let cancelled = false;
    api
      .getNodeInterfaces(nodeName)
      .then((names) => {
        if (!cancelled) setOptions(names);
      })
      .catch((err) => {
        console.error(`[egress] failed to list interfaces for ${nodeName}:`, err);
      });
    return () => {
      cancelled = true;
    };
  }, [nodeName]);

  const toggleFallback = (enabled: boolean) => {
    setFallbackEnabled(enabled);
    onChange({ ...addr, fallback: enabled ? { interface: '', health_check_ip: '' } : null });
  };

  const updateFallback = (patch: Partial<EgressFallback>) => {
    onChange({ ...addr, fallback: { ...(addr.fallback ?? { interface: '', health_check_ip: '' }), ...patch } });
  };

  return (
    <div className="card" style={{ marginBottom: 8 }}>
      <div className="section-head">
        <strong>{netName}</strong> <span className="faint">(egress)</span>
        {!disabled && (
          <button
            className="ghost"
            onClick={() => onChange({ interface: '', open_ports: [], fallback: null })}
            aria-label={`remove ${netName} from ${nodeName}`}
            title={`Remove ${netName} from this node`}
          >
            <Icon name="close" />
          </button>
        )}
      </div>
      <div className="field">
        <label htmlFor={`node-${nodeName}-${netName}-iface`}>Interface</label>
        <input
          id={`node-${nodeName}-${netName}-iface`}
          className="mono"
          type="text"
          list={listId}
          value={addr.interface}
          onChange={(e) => onChange({ ...addr, interface: e.target.value })}
          disabled={disabled}
        />
        <datalist id={listId}>
          {options.map((name) => (
            <option key={name} value={name} />
          ))}
        </datalist>
        <span className="field-hint">
          The host interface this node's outbound traffic on this network leaves through, e.g. a
          VPN tunnel or the node's WAN interface. Must already exist on the node — clouduler does
          not bring it up.
        </span>
      </div>
      <CheckboxField
        id={`node-${nodeName}-${netName}-fallback-enabled`}
        label="Fall back to a second interface"
        value={fallbackEnabled}
        onChange={toggleFallback}
        hint="If this node cannot reach an IP through the interface above for one reconcile tick, route this network out a second interface instead until the first one answers again."
        disabled={disabled}
      />
      {fallbackEnabled && (
        <>
          <div className="field">
            <label htmlFor={`node-${nodeName}-${netName}-fallback-iface`}>Fallback interface</label>
            <input
              id={`node-${nodeName}-${netName}-fallback-iface`}
              className="mono"
              type="text"
              list={listId}
              value={addr.fallback?.interface ?? ''}
              onChange={(e) => updateFallback({ interface: e.target.value })}
              disabled={disabled}
            />
          </div>
          <IpField
            id={`node-${nodeName}-${netName}-fallback-healthcheck`}
            label="Health check IP"
            value={addr.fallback?.health_check_ip ?? ''}
            onChange={(v) => updateFallback({ health_check_ip: v })}
            hint="Checked by pinging it out the primary interface, once per reconcile tick. Pick something reliably up, e.g. 1.1.1.1."
            disabled={disabled}
          />
        </>
      )}
    </div>
  );
}

/**
 * A wireguard-kind network is not addressable by an IP or interface name the
 * admin picks: importing a config seals it as a secret and clouduler derives
 * the tunnel interface name itself, returned by the import call and stored
 * as this node's address on the network. Applies immediately, like
 * `SecretForm`, rather than waiting on the draft's own Save.
 */
function WireguardAddressField({
  nodeName,
  netName,
  addr,
  onChange,
  onCancel,
  disabled,
}: {
  nodeName: string;
  netName: string;
  addr: InterfaceAddress | null;
  onChange: (addr: InterfaceAddress | null) => void;
  /** Drops this network from the node before any config has been imported
   *  for it (i.e. undoes \"Add network\" rather than removing a live one). */
  onCancel?: () => void;
  disabled: boolean;
}) {
  const [mode, setMode] = useState<'paste' | 'file'>('paste');
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const onFile = async (file: File) => setText(await file.text());

  const importConfig = async () => {
    setError(null);
    setBusy(true);
    try {
      const result = await api.putWireguardConfig(netName, nodeName, encodeText(text));
      onChange({ interface: result.interface, open_ports: [] });
      setText('');
    } catch (err) {
      console.error(`[wireguard] failed to import config for ${nodeName}/${netName}:`, err);
      setError(err instanceof ApiError ? err.message : 'failed to import wireguard config');
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    if (!confirm(`Remove the imported WireGuard config for ${nodeName} on ${netName}?`)) return;
    setError(null);
    setBusy(true);
    try {
      await api.removeWireguardConfig(netName, nodeName);
      onChange(null);
    } catch (err) {
      setError(err instanceof ApiError ? err.message : 'failed to remove wireguard config');
    } finally {
      setBusy(false);
    }
  };

  if (addr) {
    return (
      <div className="card" style={{ marginBottom: 8 }}>
        <strong>{netName}</strong> <span className="faint">(wireguard)</span>
        <p className="field-hint">
          Imported. Tunnel interface: <span className="mono">{addr.interface}</span>
        </p>
        {error && (
          <p className="field-error">
            <Icon name="alert-circle" /> {error}
          </p>
        )}
        <OpenPortsEditor
          idPrefix={`node-${nodeName}-${netName}`}
          hint="Ports the tunnel's far end forwards back to this node, e.g. a VPN provider's port forwarding feature. A raw port a stack exposes on this network only ever schedules onto a node that lists it."
          ports={addr.open_ports}
          onChange={(open_ports) => onChange({ interface: addr.interface, open_ports })}
          disabled={disabled}
        />
        {!disabled && (
          <button className="danger" onClick={remove} disabled={busy}>
            <Icon name="trash" /> Remove
          </button>
        )}
      </div>
    );
  }

  return (
    <div className="card" style={{ marginBottom: 8 }}>
      <div className="section-head">
        <span>
          <strong>{netName}</strong> <span className="faint">(wireguard)</span>
        </span>
        {!disabled && onCancel && (
          <button
            className="ghost"
            onClick={onCancel}
            aria-label={`remove ${netName} from ${nodeName}`}
            title={`Remove ${netName} from this node`}
          >
            <Icon name="close" />
          </button>
        )}
      </div>
      <div className="field">
        <label>Config source</label>
        <div className="segmented" role="tablist">
          <button type="button" role="tab" aria-selected={mode === 'paste'} onClick={() => setMode('paste')}>
            <Icon name="edit" /> Paste
          </button>
          <button type="button" role="tab" aria-selected={mode === 'file'} onClick={() => setMode('file')}>
            <Icon name="file" /> File
          </button>
        </div>
      </div>
      {mode === 'file' && (
        <div className="field">
          <label htmlFor={`node-${nodeName}-${netName}-wg-file`}>File</label>
          <input
            id={`node-${nodeName}-${netName}-wg-file`}
            type="file"
            onChange={(e) => e.target.files?.[0] && onFile(e.target.files[0])}
            disabled={disabled}
          />
        </div>
      )}
      <div className="field">
        <label htmlFor={`node-${nodeName}-${netName}-wg-text`}>wg-quick config</label>
        <textarea
          id={`node-${nodeName}-${netName}-wg-text`}
          rows={8}
          value={text}
          onChange={(e) => setText(e.target.value)}
          placeholder={'[Interface]\nPrivateKey = ...\nAddress = ...\n\n[Peer]\nPublicKey = ...\nEndpoint = ...\nAllowedIPs = 0.0.0.0/0'}
          disabled={disabled}
        />
      </div>
      {error && (
        <p className="field-error">
          <Icon name="alert-circle" /> {error}
        </p>
      )}
      {!disabled && (
        <button type="button" onClick={importConfig} disabled={busy || !text.trim()}>
          <Icon name="check" /> Import
        </button>
      )}
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
        hint="Picks which node address the dashboard's DNS record points at, and whose domain the Address label is qualified with. Doesn't restrict which hostname you can use."
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
        hint="How long a node can go unreachable before the leader marks it unreachable and reschedules its work elsewhere, e.g. 30s. Must be longer than the fence timeout, so a node has fenced itself before the rest of the cluster gives up on it."
        disabled={disabled}
      />
      <DurationField
        id="fo-fence"
        label="Fence timeout"
        value={f.fence_timeout}
        onChange={(fence_timeout) => set({ fence_timeout })}
        disabled={disabled}
        hint="How long a node can go without raft quorum before it stops its own containers, e.g. 15s. Must be shorter than the node timeout and longer than the probe interval; this ordering is what stops a replicated volume from ending up with two writers."
      />
      <DurationField
        id="fo-probe"
        label="Probe interval"
        value={f.probe_interval}
        onChange={(probe_interval) => set({ probe_interval })}
        hint="How often the leader probes every node's reachability, e.g. 5s. Must be shorter than the fence timeout, so a node cannot be declared dead before it has had the chance to fence itself."
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
        hint="Cluster-wide default for whether a node may create the secret-sealing key if it doesn't have one yet. Each node can override this under its own Nodes page card, e.g. to forbid it on a node joining an existing cluster."
        disabled={disabled}
      />
    </div>
  );
}

const DEFAULT_GIT_BACKUP: NonNullable<ClusterConfig['backup']> = {
  repo_url: '',
  age_recipient: '',
  branch: 'main',
  interval: '30s',
};

/** Generates a fresh age (X25519) keypair entirely client-side, downloads the
 *  private half as a file, and hands back the public recipient string to
 *  autofill the field above. The private key never touches the server -
 *  clouduler only ever needs the recipient to encrypt secrets for. */
async function generateAndDownloadAgeKey(): Promise<string> {
  const identity = await generateX25519Identity();
  const recipient = await identityToRecipient(identity);

  const contents = `# created: ${new Date().toISOString()}\n# public key: ${recipient}\n${identity}\n`;
  const blob = new Blob([contents], { type: 'text/plain' });
  const url = URL.createObjectURL(blob);
  try {
    const a = document.createElement('a');
    a.href = url;
    a.download = 'clouduler-age-key.txt';
    document.body.appendChild(a);
    a.click();
    a.remove();
  } finally {
    URL.revokeObjectURL(url);
  }

  return recipient;
}

export function GitBackupSection({ draft, setDraft, disabled }: SectionProps) {
  const b = draft.backup;
  const set = (patch: Partial<NonNullable<ClusterConfig['backup']>>) =>
    setDraft({ ...draft, backup: b ? { ...b, ...patch } : { ...DEFAULT_GIT_BACKUP, ...patch } });
  const [generating, setGenerating] = useState(false);
  const [generateError, setGenerateError] = useState<string | null>(null);

  const generateKey = async () => {
    setGenerateError(null);
    setGenerating(true);
    try {
      const age_recipient = await generateAndDownloadAgeKey();
      set({ age_recipient });
    } catch (err) {
      setGenerateError(err instanceof Error ? err.message : 'failed to generate age key');
    } finally {
      setGenerating(false);
    }
  };

  return (
    <div className="card">
      <CheckboxField
        id="backup-enabled"
        label="Mirror cluster state into a git repository"
        value={b != null}
        onChange={(enabled) => setDraft({ ...draft, backup: enabled ? DEFAULT_GIT_BACKUP : null })}
        hint="The leader periodically commits and pushes cluster.yaml, stack specs, configs and secrets to this repo, so it can be rebuilt from git after a total loss. Secrets are re-encrypted for the age key below - the cluster's own key never leaves it."
        disabled={disabled}
      />
      {b && (
        <>
          <TextField
            id="backup-repo-url"
            label="Repository URL"
            value={b.repo_url}
            onChange={(repo_url) => set({ repo_url })}
            hint="An SSH url, e.g. git@github.com:org/repo.git. Push auth uses a deploy key clouduler generates itself - add its public half as a write-capable deploy key on the repo once it appears below."
            disabled={disabled}
          />
          <TextField
            id="backup-age-recipient"
            label="Age recipient"
            value={b.age_recipient}
            onChange={(age_recipient) => set({ age_recipient })}
            hint="The admin's age (X25519) public key, e.g. age1.... Secrets are encrypted for this key before being committed; only whoever holds the matching private key, generated and kept outside clouduler entirely, can read them back."
            disabled={disabled}
          />
          <div className="btn-row" style={{ marginTop: -8, marginBottom: 16 }}>
            <button type="button" onClick={generateKey} disabled={disabled || generating}>
              <Icon name="download" /> {generating ? 'Generating...' : 'Generate age key'}
            </button>
            <span className="field-hint">
              Generates a keypair in your browser and downloads the private key - it is never sent to the server.
              The public recipient above is filled in automatically; store the downloaded file somewhere safe, it is
              the only copy.
            </span>
          </div>
          {generateError && (
            <p className="field-error">
              <Icon name="alert-circle" /> {generateError}
            </p>
          )}
          <TextField
            id="backup-branch"
            label="Branch"
            value={b.branch}
            onChange={(branch) => set({ branch })}
            disabled={disabled}
          />
          <DurationField
            id="backup-interval"
            label="Export interval"
            value={b.interval}
            onChange={(interval) => set({ interval })}
            hint="How often the leader checks for changes to export, e.g. 30s. A tick with nothing new to commit is a no-op."
            disabled={disabled}
          />
          <GitBackupDeployKey />
        </>
      )}
    </div>
  );
}

/** Shows the leader-generated deploy key and last export status, polled once
 *  on mount - this is read-only reporting, not part of the draft, so it does
 *  not need to track edits like the fields above. */
function GitBackupDeployKey() {
  const [status, setStatus] = useState<GitBackupStatus | null>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    api.gitBackupStatus().then(setStatus).catch(() => setStatus(null));
  }, []);

  const copy = async () => {
    if (!status?.deploy_public_key) return;
    await navigator.clipboard.writeText(status.deploy_public_key);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };

  if (!status) {
    return (
      <p className="loading">
        <span className="spinner" /> Checking the deploy key...
      </p>
    );
  }

  return (
    <div className="field">
      <label>Deploy key</label>
      {status.deploy_public_key ? (
        <>
          <span className="field-hint">
            Add this as a write-capable deploy key on the repository above; the leader generated it and holds the
            private half.
          </span>
          <pre className="mono">{status.deploy_public_key}</pre>
          <div className="btn-row" style={{ marginTop: 8 }}>
            <button type="button" onClick={copy}>
              <Icon name={copied ? 'check' : 'copy'} /> {copied ? 'Copied' : 'Copy public key'}
            </button>
          </div>
        </>
      ) : (
        <span className="field-hint">
          Not generated yet - the leader creates one on its first export tick after saving a repository url above.
        </span>
      )}
      {status.last_export && (
        <p className={status.last_export.error ? 'field-error' : 'muted'} style={{ marginTop: 8 }}>
          {status.last_export.error ? (
            <>
              <Icon name="alert-circle" /> Last export failed: {status.last_export.error}
            </>
          ) : (
            <>Last export: {status.last_export.summary}</>
          )}
        </p>
      )}
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
        hint="How long a browser session stays signed in before it has to log in again, e.g. 12h."
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
                  scopes: ['openid', 'profile', 'email'],
                  admin_claim: 'groups',
                  admin_value: null,
                  username_claim: 'preferred_username',
                }
              : null,
          })
        }
        hint="Register clouduler as an app with your identity provider (Authentik, Keycloak, Google, Okta, etc.) first, with its redirect/callback URL set to https://<the dashboard's address>/auth/oidc/callback, which is the address configured on the Web page."
        disabled={disabled}
      />
      {a.oidc && (
        <div className="card" style={{ marginTop: 8 }}>
          <TextField
            id="oidc-issuer"
            label="Issuer"
            value={a.oidc.issuer}
            onChange={(issuer) => set({ oidc: { ...a.oidc!, issuer } })}
            hint="Your provider's base URL. Must start with https://. Discovery is fetched from <issuer>/.well-known/openid-configuration, e.g. https://accounts.google.com or https://your-tenant.eu.auth0.com."
            disabled={disabled}
          />
          <TextField
            id="oidc-clientid"
            label="Client ID"
            value={a.oidc.client_id}
            onChange={(client_id) => set({ oidc: { ...a.oidc!, client_id } })}
            hint="The client/application ID your provider issued when you registered clouduler as an app."
            disabled={disabled}
          />
          {/* The client secret is its own sealed secret, independent of `a.oidc`
              and the rest of the draft (see OidcClientSecret). Kept settable
              even while OIDC is being configured, so a secret can be staged
              ahead of turning it on. */}
          <OidcClientSecret disabled={disabled} />
          <TextField
            id="oidc-usernameclaim"
            label="Username claim"
            value={a.oidc.username_claim}
            onChange={(username_claim) => set({ oidc: { ...a.oidc!, username_claim } })}
            hint="Name of the claim shown in the web ui as the signed-in user's name, e.g. preferred_username, email, or name. Falls back to the opaque subject id when this claim is absent."
            disabled={disabled}
          />
          <TextField
            id="oidc-adminclaim"
            label="Admin claim"
            value={a.oidc.admin_claim}
            onChange={(admin_claim) => set({ oidc: { ...a.oidc!, admin_claim } })}
            hint="Name of the claim in the ID token that lists the user's groups or roles, e.g. groups. Check what your provider actually sends, as it varies (roles, groups, https://your-app/roles, ...)."
            disabled={disabled}
          />
          <TextField
            id="oidc-adminvalue"
            label="Admin value"
            value={a.oidc.admin_value ?? ''}
            onChange={(v) => set({ oidc: { ...a.oidc!, admin_value: v || null } })}
            hint="A user whose admin claim contains this value gets the admin role; everyone else who signs in successfully gets read-only viewer access. Leave blank and nobody gets admin through OIDC."
            disabled={disabled}
          />
        </div>
      )}
    </div>
  );
}

/**
 * The client secret, set here rather than by naming a file the operator has
 * to create over ssh first.
 *
 * It sits outside the config draft on purpose, same reasoning as
 * `CloudflareToken`. The secret is written by its own endpoint the moment
 * Save is pressed, not on Apply, because it is not a config value that can be
 * diffed or rolled back. Sealed and replicated as a cluster secret, so every
 * node has it, and can be rotated at any time by submitting a new one; it
 * just overwrites the old.
 */
function OidcClientSecret({ disabled }: { disabled: boolean }) {
  const [status, setStatus] = useState<OidcClientSecretStatus | null>(null);
  const [secret, setSecret] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.oidcClientSecret().then(setStatus).catch(() => setStatus(null));
  }, []);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.setOidcClientSecret(secret.trim());
      setSecret('');
      setStatus(await api.oidcClientSecret());
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'could not save the client secret');
    } finally {
      setBusy(false);
    }
  };

  if (!status) {
    return (
      <p className="loading">
        <span className="spinner" /> Checking the client secret...
      </p>
    );
  }

  return (
    <div className="field">
      <label htmlFor="oidc-secret">Client secret</label>
      <span className="field-hint">
        The client secret your provider issued alongside the client ID. Leave unset only if your provider supports a
        public/secretless client.
      </span>
      {status.configured && (
        <p className="muted">
          A secret is stored, replicated to every node. It cannot be shown again; enter a new one below to rotate it.
        </p>
      )}
      <input
        id="oidc-secret"
        type="password"
        value={secret}
        onChange={(e) => setSecret(e.target.value)}
        placeholder="OIDC client secret"
        autoComplete="off"
        disabled={disabled || busy}
      />
      {!status.configured && (
        <span className="field-hint">
          Sealed and stored as soon as you press Save, separately from the rest of this page. You will not be able to
          read it back.
        </span>
      )}
      {error && (
        <span className="field-error">
          <Icon name="alert-circle" /> {error}
        </span>
      )}
      <div className="btn-row" style={{ marginTop: 8 }}>
        <button onClick={save} disabled={disabled || busy || secret.trim().length === 0}>
          {busy ? <span className="spinner" /> : <Icon name="key" />}
          {busy ? 'Saving...' : status.configured ? 'Rotate secret' : 'Save secret'}
        </button>
      </div>
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
        hint="Terminates HTTP/HTTPS for every service route declared under a stack's x-clouduler.proxy and forwards to its container. Both ports below are bound on every node, on every address, since a route follows its container as it moves."
        disabled={disabled}
      />
      <PortField
        id="proxy-http-port"
        label="HTTP port"
        value={p.http_port}
        onChange={(http_port) => set({ http_port })}
        hint="Usually 80. Below 1024, so the daemon needs root to bind it."
        disabled={disabled || !p.enabled}
      />
      <PortField
        id="proxy-https-port"
        label="HTTPS port"
        value={p.https_port}
        onChange={(https_port) => set({ https_port })}
        hint="Usually 443. Must differ from the web ui's port, which listens on the same addresses."
        disabled={disabled || !p.enabled}
      />
      <CheckboxField
        id="proxy-acme-enabled"
        label="Issue certificates with ACME"
        value={p.acme.enabled}
        onChange={(enabled) => set({ acme: { ...p.acme, enabled } })}
        hint="Automatically obtains and renews TLS certificates via ACME DNS-01 for every published route's hostname. Requires Cloudflare DNS management to be configured below, as DNS-01 is the only challenge type supported."
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
        onChange={(enabled) => setDraft({ ...draft, cloudflare: enabled ? { zones: [] } : null })}
        hint="Lets clouduler write DNS records for network and route domains, and is required if ACME certificate issuance is turned on above (DNS-01 challenges go through Cloudflare)."
        disabled={disabled}
      />
      {cf && (
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
          hint="Zone names clouduler may write records to, e.g. example.com, other-example.com. Every network domain and the dashboard's own address must fall under one of these zones."
          disabled={disabled}
        />
      )}
      {/* The token is its own sealed secret, independent of `cf` and the rest
          of the draft (see CloudflareToken). Kept settable even with DNS
          management off, so a token can be staged ahead of flipping it on. */}
      <CloudflareToken disabled={disabled} />
    </div>
  );
}

/**
 * The API token, set here rather than by naming a file the operator has to
 * create over ssh first.
 *
 * It sits outside the config draft on purpose. The token is written by its own
 * endpoint the moment Save is pressed, not on Apply, because it is not a config
 * value that can be diffed or rolled back. Sealed and replicated as a cluster
 * secret, so every node has it, and can be rotated at any time by submitting a
 * new one; it just overwrites the old.
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

  return (
    <div className="field">
      <label htmlFor="cf-token">API token</label>
      <span className="field-hint">
        Create one at the Cloudflare dashboard under My Profile &gt; API Tokens, scoped to Zone / DNS / Edit for the
        zones listed above.
      </span>
      {status.configured && (
        <p className="muted">
          A token is stored, replicated to every node. It cannot be shown again; enter a new one below to rotate it.
        </p>
      )}
      <input
        id="cf-token"
        type="password"
        value={token}
        onChange={(e) => setToken(e.target.value)}
        placeholder="Cloudflare API token"
        autoComplete="off"
        disabled={disabled || busy}
      />
      {!status.configured && (
        <span className="field-hint">
          Sealed and stored as soon as you press Save, separately from the rest of this page. You will not be able to
          read it back.
        </span>
      )}
      {error && (
        <span className="field-error">
          <Icon name="alert-circle" /> {error}
        </span>
      )}
      <div className="btn-row" style={{ marginTop: 8 }}>
        <button onClick={save} disabled={disabled || busy || token.trim().length === 0}>
          {busy ? <span className="spinner" /> : <Icon name="key" />}
          {busy ? 'Saving...' : status.configured ? 'Rotate token' : 'Save token'}
        </button>
      </div>
    </div>
  );
}
