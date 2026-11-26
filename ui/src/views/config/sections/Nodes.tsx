import { useState } from 'react';
import type { ClusterConfig, InterfaceAddress, NetworkConfig, NodeAddress, NodeConfig, PublicAddress } from '../../../api/types';
import { Disclosure, Tag } from '../../../ds';
import { Icon } from '../../../components/Icon';
import { CheckboxField, IpField, KeyValueList, PortField, SelectField, TextField } from '../../../components/FormFields';
import { isManaged, managedNodeAddressError, nodeAddressOn } from '../../../lib/managedNetwork';
import { OpenPortsEditor, EgressInterfaceField, WireguardAddressField } from './NetworkFields';

const NET_KIND_LABEL: Record<string, string> = { internal: 'internal', public: 'public', egress: 'egress', wireguard: 'wireguard', private: 'private' };

function netKind(net: NetworkConfig): string {
  return isManaged(net) ? 'managed' : (NET_KIND_LABEL[net.type] ?? net.type);
}

/** One-line address and port list of a node's address on a network, for the
 *  collapsed row. */
function netSummary(addr: NodeAddress | undefined): { address: string; ports: string } {
  if (addr === undefined) return { address: '', ports: '' };
  if (addr === null) return { address: 'access only', ports: '' };
  if (typeof addr === 'string') return { address: addr, ports: '' };
  const ports = addr.open_all
    ? 'all'
    : (addr.open_ports ?? []).map((p) => `${p.port}/${p.protocol}`).join(', ');
  if ('ip' in addr) return { address: addr.ip, ports };
  if ('interface' in addr) {
    const fb = addr.fallback ? ` (backup ${addr.fallback})` : '';
    return { address: addr.interface ? `${addr.interface}${fb}` : '', ports };
  }
  return { address: [addr.ipv4, addr.ipv6, addr.domain].filter(Boolean).join(', '), ports };
}

/** A node's config-level card: role, ports, addresses and drive mounts. Used
 *  standalone on a node's detail page (see `NodeDetail`), scoped to just that
 *  one node rather than listed alongside every other node. */
export function NodeCard({
  name,
  node,
  networks,
  drives,
  cluster,
  onChange,
  onRemove,
  disabled,
}: {
  name: string;
  node: NodeConfig;
  networks: ClusterConfig['networks'];
  /** The whole draft, for managed-network address autofill and checks across nodes. */
  cluster?: ClusterConfig;
  drives: ClusterConfig['drives'];
  onChange: (patch: Partial<NodeConfig>) => void;
  onRemove: () => void;
  disabled: boolean;
}) {
  const drivePairs: [string, string][] = Object.entries(node.drives);
  const fullDraft = (cluster ?? { networks, nodes: { [name]: node } }) as ClusterConfig;

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
      net.type !== 'container' &&
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
    // A managed network starts with an automatic node address (null); an
    // explicit one is optional.
    const value: NodeAddress | null =
      net.type === 'public'
        ? { ipv4: null, ipv6: null, domain: null, open_ports: [] }
        : isManaged(net)
          ? null
          : '';
    onChange({ network: { ...node.network, [netName]: value } });
  };

  // Networks this node has an editor for: internal always, wireguard once
  // added or pending, every other addressable kind only when listed.
  const shownNetworks = networkEntries.filter(([netName, net]) => {
    if (net.type === 'nat' || net.type === 'container') return false;
    if (net.type === 'internal') return true;
    return netName in node.network || (net.type === 'wireguard' && pendingWireguard.includes(netName));
  });
  const [openNets, setOpenNets] = useState<string[]>([]);

  const renderNetwork = (netName: string, net: NetworkConfig) => {
      const addr = node.network[netName];
      if (net.type === 'nat' || net.type === 'container') {
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
              ? {
                  interface: addr.interface,
                  open_ports: addr.open_ports ?? [],
                  ...(addr.open_all ? { open_all: true } : {}),
                }
              : null;
        return (
          <WireguardAddressField
            key={netName}
            nodeName={name}
            netName={netName}
            addr={wg}
            onChange={(v) => setAddress(netName, v ? { interface: v.interface, open_ports: v.open_ports, ...(v.open_all ? { open_all: true } : {}) } : null)}
            onCancel={
              wg === null ? () => setPendingWireguard((prev) => prev.filter((n) => n !== netName)) : undefined
            }
            disabled={disabled}
          />
        );
      }
      if (net.type === 'public') {
        const pub = (addr !== null && typeof addr === 'object' ? addr : {}) as Partial<PublicAddress>;
        const update = (patch: Partial<PublicAddress>) => {
          const merged: PublicAddress = {
            ipv4: pub.ipv4 ?? null,
            ipv6: pub.ipv6 ?? null,
            domain: pub.domain ?? null,
            open_ports: pub.open_ports ?? [],
            ...(pub.open_all ? { open_all: true } : {}),
            ...patch,
          };
          if (!merged.open_all) delete merged.open_all;
          const empty =
            !merged.ipv4 && !merged.ipv6 && !merged.domain && merged.open_ports.length === 0 && !merged.open_all;
          setAddress(netName, empty ? null : merged);
        };
        return (
          <div key={netName} className="card" style={{ marginBottom: 8 }}>
            <div className="section-head">
              <span className="faint">Public address</span>
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
              label="Public IPv4"
              value={pub.ipv4 ?? ''}
              onChange={(v) => update({ ipv4: v || null })}
              hint="Publishes an A record."
              help="This node's public IPv4 address on this network. Publishes an A record if the network has a domain."
              disabled={disabled}
            />
            <TextField
              id={`node-${name}-${netName}-ipv6`}
              label="Public IPv6"
              value={pub.ipv6 ?? ''}
              onChange={(v) => update({ ipv6: v || null })}
              hint="Publishes an AAAA record."
              help="This node's public IPv6 address on this network. Publishes an AAAA record if the network has a domain."
              disabled={disabled}
            />
            <TextField
              id={`node-${name}-${netName}-domain`}
              label="Alias hostname"
              value={pub.domain ?? ''}
              onChange={(v) => update({ domain: v || null })}
              hint="Points DNS at a hostname instead of an IP."
              help="Points this node's DNS record at another hostname instead of a fixed IP, e.g. for a dynamic-DNS address."
              disabled={disabled}
            />
            <OpenPortsEditor
              idPrefix={`node-${name}-${netName}`}
              hint="Ports this node's router forwards to it here. A raw port a stack exposes on this network only ever schedules onto a node that lists it."
              ports={pub.open_ports ?? []}
              onChange={(open_ports) => update({ open_ports })}
              openAll={!!pub.open_all}
              onOpenAllChange={(open_all) => update({ open_all })}
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
              ? {
                  interface: addr.interface,
                  open_ports: addr.open_ports ?? [],
                  ...(addr.open_all ? { open_all: true } : {}),
                  fallback: addr.fallback ?? null,
                }
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
      const ip = typeof addr === 'string' ? addr : addr && 'ip' in addr ? addr.ip : '';
      const privatePorts = addr && typeof addr === 'object' && 'ip' in addr ? addr.open_ports : [];
      // A private network can be listed with no address: the node routes into
      // it for access but hosts no containers there (serialized as null).
      const managedNet = isManaged(net);
      const accessOnly = net.type === 'private' && !managedNet && addr === null;
      const autoAddr = managedNet && addr === null ? nodeAddressOn(fullDraft, name, netName) : null;
      return (
        <div key={netName} className="flex-row" style={{ gap: 8, alignItems: 'flex-end' }}>
          <div style={{ flex: 1 }}>
            {net.type === 'private' && managedNet && (
              <CheckboxField
                id={`node-${name}-${netName}-auto-address`}
                label="Automatic address"
                value={addr === null}
                onChange={(v) => onChange({ network: { ...node.network, [netName]: v ? null : '' } })}
                hint={autoAddr ? `Currently ${autoAddr} (picked from the top of the subnet).` : 'Picked from the top of the subnet, stable per node.'}
                help="Every node that lists a managed network holds a node address on it: this is where outside clients reach the services of containers running on this node (open ports, and the reverse proxy when enabled), and where DNS points for them. Automatic picks a stable address from the top of the subnet (containers use the bottom, the first host address is the gateway). Untick it to choose the address yourself."
                configKey={`nodes.${name}.network.${netName}`}
                disabled={disabled}
              />
            )}
            {net.type === 'private' && !managedNet && (
              <CheckboxField
                id={`node-${name}-${netName}-access-only`}
                label="Access only (no address)"
                value={accessOnly}
                onChange={(v) => onChange({ network: { ...node.network, [netName]: v ? null : '' } })}
                hint="Route into this network without an address of its own."
                help="The node implements the network (it routes and NATs traffic into it) but holds no address on it and never hosts containers on it. Use it for a node that only gives clients access to the network."
                configKey={`nodes.${name}.network.${netName}`}
                disabled={disabled}
              />
            )}
            {!accessOnly && !autoAddr && !(managedNet && addr === null) && <IpField
              id={`node-${name}-${netName}-ip`}
              label={`Address on ${netName}`}
              hint={
                net.type === 'internal'
                  ? 'Required, inside the subnet.'
                  : isManaged(net)
                    ? 'Inside the subnet; not the network, broadcast or gateway address, and not a container address.'
                    : 'Inside the subnet.'
              }
              help={
                net.type === 'internal'
                  ? "This node's fixed address on the internal network, inside its subnet. Required for every node because raft, drive sync and the container overlay all use it."
                  : isManaged(net)
                    ? "This node's fixed address on the managed network, inside its subnet. It is where outside clients reach the services of containers on this node and where DNS for them points. Must not be the network, broadcast or gateway address."
                    : "This node's fixed address on this network, inside its subnet."
              }
              configKey={`nodes.${name}.network.${netName}`}
              value={ip}
              onChange={(v) =>
                setAddress(netName, !v ? (managedNet ? '' : null) : privatePorts.length > 0 ? { ip: v, open_ports: privatePorts } : v)
              }
              disabled={disabled}
            />}
            {net.type === 'private' && !managedNet && !accessOnly && (
              <OpenPortsEditor
                idPrefix={`node-${name}-${netName}`}
                hint="Ports this node answers on its address here. A raw port a stack exposes on this network only schedules onto a node that lists it, once any node lists a port; a network where no node lists one is not gated."
                ports={privatePorts}
                onChange={(open_ports) => setAddress(netName, open_ports.length > 0 && ip ? { ip, open_ports } : ip)}
                disabled={disabled || !ip}
              />
            )}
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
          {isManaged(net) && (() => {
            const err = managedNodeAddressError(fullDraft, name, netName);
            return err ? (
              <p className="field-error" style={{ flexBasis: '100%' }}>
                <Icon name="alert-circle" /> {err}
              </p>
            ) : null;
          })()}
        </div>
      );
  };

  return (
    <div className="card section node-cfg">
      <div className="section-head node-cfg__head">
        <h3 className="flex-row">
          <Icon name="nodes" /> {name}
        </h3>
        <Tag>{node.role}</Tag>
        {!disabled && (
          <button className="danger node-cfg__remove" onClick={onRemove}>
            <Icon name="trash" /> Remove node
          </button>
        )}
      </div>
      <Disclosure title="General" defaultOpen>
      <SelectField
        id={`node-${name}-role`}
        label="Node role"
        configKey={`nodes.${name}.role`}
        value={node.role}
        onChange={(role) => onChange({ role: role as 'manager' | 'worker' })}
        options={[
          { value: 'manager', label: 'Manager' },
          { value: 'worker', label: 'Worker' },
        ]}
        hint="Managers vote; workers only run stacks."
        help="Managers take part in raft consensus and can become leader; workers only run stacks. A cluster needs at least one manager, and an odd number of managers to keep quorum decisive."
        disabled={disabled}
      />
      <PortField
        id={`node-${name}-port`}
        label="Cluster RPC port"
        configKey={`nodes.${name}.rpc_port`}
        value={node.rpc_port}
        onChange={(rpc_port) => onChange({ rpc_port })}
        hint="Other nodes dial this port."
        help="The port other nodes dial, on this node's internal-network address, to reach its cluster RPC service."
        disabled={disabled}
      />
      <TextField
        id={`node-${name}-datadir`}
        label="Data directory"
        configKey={`nodes.${name}.data_dir`}
        value={node.data_dir ?? ''}
        onChange={(data_dir) => onChange({ data_dir: data_dir || null })}
        hint="Blank uses the cluster default."
        help="Overrides the cluster-wide data directory for this node only. Leave blank to use the cluster default."
        disabled={disabled}
      />
      <TextField
        id={`node-${name}-docker-datadir`}
        label="Docker data directory"
        configKey={`nodes.${name}.docker_data_dir`}
        value={node.docker_data_dir ?? ''}
        onChange={(docker_data_dir) => onChange({ docker_data_dir: docker_data_dir || null })}
        hint="Blank uses docker's default."
        help="Where this node's docker daemon stores images and containers. Leave blank for docker's own default (/var/lib/docker). Changing this only takes effect once docker is restarted, and existing data under the old location is not moved."
        disabled={disabled}
      />
      {/* A node that joined an existing cluster is set to "never", because a
          second cluster key would only seal secrets nothing else can open. */}
      <SelectField
        id={`node-${name}-genkey`}
        label="Cluster key creation"
        configKey={`nodes.${name}.generate_secret_key`}
        value={
          node.generate_secret_key === null || node.generate_secret_key === undefined
            ? 'inherit'
            : String(node.generate_secret_key)
        }
        onChange={(choice) =>
          onChange({
            generate_secret_key: choice === 'inherit' ? null : choice === 'true',
          })
        }
        options={[
          { value: 'inherit', label: 'Follow the cluster setting' },
          { value: 'true', label: 'Yes, this node may create it' },
          { value: 'false', label: 'Never, it can only be given the key' },
        ]}
        hint="Only the first node of a new cluster should."
        help="Only the first node bootstrapping a brand new cluster should be allowed to generate the key. A node joining an existing cluster should be set to 'Never', since a second key could not open secrets the rest of the cluster already sealed."
        disabled={disabled}
      />
      <CheckboxField
        id={`node-${name}-last-resort`}
        label="Use only as a last resort"
        configKey={`nodes.${name}.last_resort`}
        value={node.last_resort}
        onChange={(last_resort) => onChange({ last_resort })}
        hint="Gets work only when nothing else fits."
        help="Only place a stack here when no other eligible node can take it. Useful for a node whose one virtue is a large drive but whose cpu/ram are weak, so the scheduler doesn't pile general workload onto it just because it sorts first."
        disabled={disabled}
      />
      <CheckboxField
        id={`node-${name}-disk-probe`}
        label="Measure disk speed"
        configKey={`nodes.${name}.disk_probe`}
        value={node.disk_probe !== false}
        onChange={(disk_probe) => onChange({ disk_probe })}
        hint="Lets the node tables flag a slow disk."
        help="When the connectivity table asks this node for a report, it times a small write plus fsync in its data dir. Turn it off for a disk you do not want touched, such as an SD card. The node then reports no disk figure and is never shown as 'Slow disk'."
        disabled={disabled}
      />

      </Disclosure>

      <Disclosure title="Networks" count={shownNetworks.length} defaultOpen>
      {shownNetworks.length === 0 && <p className="field-hint">No networks on this node.</p>}
      {shownNetworks.map(([netName, net]) => {
        const body = renderNetwork(netName, net);
        if (!body) return null;
        const addr = node.network[netName];
        const sum = netSummary(addr);
        const addrErr = isManaged(net) ? managedNodeAddressError(fullDraft, name, netName) : null;
        const pendingWg = net.type === 'wireguard' && !(netName in node.network);
        return (
          <Disclosure
            key={netName}
            title={
              <>
                <strong>{netName}</strong> <Tag>{netKind(net)}</Tag>
                {addrErr && <Tag tone="danger">invalid address</Tag>}
              </>
            }
            summary={[sum.address, sum.ports].filter(Boolean).join(' · ') || (net.type === 'internal' ? 'address required' : 'not configured')}
            open={pendingWg || openNets.includes(netName)}
            onOpenChange={(o) => setOpenNets((prev) => (o ? [...prev, netName] : prev.filter((n) => n !== netName)))}
          >
            {body}
          </Disclosure>
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
                ...joinableNetworks.map(([netName]) => ({
                  value: netName,
                  label: netName,
                })),
              ]}
              disabled={disabled}
            />
          </div>
          <button
            onClick={() => {
              if (!addingNetwork) return;
              addNetworkDefault(addingNetwork);
              setOpenNets((prev) => (prev.includes(addingNetwork) ? prev : [...prev, addingNetwork]));
              setAddingNetwork('');
            }}
            disabled={!addingNetwork}
          >
            <Icon name="plus" /> Add
          </button>
        </div>
      )}

      </Disclosure>

      <Disclosure title="Drives" count={drivePairs.length}>
      <KeyValueList
        entries={drivePairs}
        onChange={(entries) => onChange({ drives: Object.fromEntries(entries.filter(([k]) => k)) })}
        keyLabel="Drive"
        valueLabel="Local path"
        keyOptions={Object.keys(drives)}
        disabled={disabled}
      />
      </Disclosure>
    </div>
  );
}
