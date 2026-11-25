import { useState } from 'react';
import { api, ApiError } from '../../../api/client';
import type { ClusterConfig, NetworkConfig, NetworkKind } from '../../../api/types';
import { canServeDns, isLastServed, isServingDns, removeDnsNetwork, renameDnsNetwork, toggleDnsNetwork } from '../dnsServerNetworks';
import { BottomSheet } from '../../../ds';
import { NetworkGraph } from '../NetworkGraph';
import { canHaveAccess, canHaveShortcut, shortcutOn, validTargets, withShortcut } from '../networkAccess';
import { Icon } from '../../../components/Icon';
import { CheckboxField, CidrField, SelectField, TextField } from '../../../components/FormFields';
import { managedNetworkError } from '../../../lib/managedNetwork';
import type { SectionProps } from './types';
import { emptyContainer, MultiPick, ContainerNetworkForm, ManagedOpenPortsEditor } from './NetworkFields';

/** Applies the same key rename the server does, to every place a network
 *  name appears in a `ClusterConfig`, so the draft stays in sync with a
 *  `renameNetwork` call without wiping any other unrelated staged edit. */
export function renameNetworkInDraft(draft: ClusterConfig, oldName: string, newName: string): ClusterConfig {
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

  for (const [n, c] of Object.entries(networks)) {
    if (c.access?.includes(oldName)) {
      networks[n] = { ...c, access: c.access.map((a) => (a === oldName ? newName : a)) };
    }
  }

  for (const [n, c] of Object.entries(networks)) {
    if (c.shortcut && oldName in c.shortcut) {
      const { [oldName]: v, ...rest } = c.shortcut;
      networks[n] = { ...c, shortcut: { ...rest, [newName]: v } };
    }
  }

  const dns = draft.proxy?.dns_server;
  const proxy =
    dns && dns.networks.includes(oldName)
      ? { ...draft.proxy, dns_server: { ...dns, networks: renameDnsNetwork(dns.networks, oldName, newName) } }
      : draft.proxy;

  return {
    ...draft,
    networks,
    nodes,
    proxy,
    web: draft.web.network === oldName ? { ...draft.web, network: newName } : draft.web,
  };
}

export function NetworksSection({ draft, setDraft, disabled, original }: SectionProps) {
  const [selected, setSelected] = useState<string | null>(null);

  const addNetwork = () => {
    let name = 'network';
    let i = 1;
    while (draft.networks[name]) name = `network${i++}`;
    setDraft({
      ...draft,
      networks: {
        ...draft.networks,
        [name]: { type: 'private', subnet: null, domain: null },
      },
    });
    setSelected(name);
  };

  const setAccess = (from: string, access: string[] | undefined) => {
    const next = { ...draft.networks[from] };
    if (access === undefined) delete next.access;
    else next.access = access;
    setDraft({ ...draft, networks: { ...draft.networks, [from]: next } });
  };

  const setShortcut = (from: string, to: string, on: boolean) => {
    const next = { ...draft.networks[from] };
    const sc = withShortcut(next, to, on);
    if (sc) next.shortcut = sc;
    else delete next.shortcut;
    setDraft({ ...draft, networks: { ...draft.networks, [from]: next } });
  };

  const sheetName = selected && selected in draft.networks ? selected : null;
  const sheetNet = sheetName ? draft.networks[sheetName] : null;

  return (
    <>
      <NetworkGraph
        networks={draft.networks}
        editable={!disabled}
        onOpen={setSelected}
        onAddEdge={(from, to) => setAccess(from, [...(draft.networks[from].access ?? []), to].sort())}
        onRemoveEdge={(from, to) => setAccess(from, (draft.networks[from].access ?? []).filter((x) => x !== to))}
      />
      {!disabled && (
        <button onClick={addNetwork}>
          <Icon name="plus" /> Add network
        </button>
      )}
      <BottomSheet open={sheetName !== null} title={sheetName ? `${sheetName} network` : 'Network'} onClose={() => setSelected(null)}>
        {sheetName && sheetNet && (
          <>
            <NetworkForm
              key={sheetName}
              name={sheetName}
              net={sheetNet}
              draft={draft}
              setDraft={setDraft}
              disabled={disabled}
              original={original}
              onRenamed={setSelected}
              onRemoved={() => setSelected(null)}
            />
            {canHaveAccess(sheetNet.type) && (
              <div className="net-access">
                <MultiPick
                  label="Can reach"
                  options={validTargets(draft.networks, sheetName)}
                  selected={sheetNet.access ?? []}
                  onChange={(access) => setAccess(sheetName, access)}
                  disabled={disabled}
                  empty="No other internal, private, public or container network."
                />
                <p className="field-hint">
                  Public networks are always reachable, listed or not.{' '}
                  {sheetNet.managed === true && 'This is the source side: it decides what this managed network\'s containers may reach, not who may reach them (that is each client network\'s own list). '}
                  {sheetNet.access == null
                    ? 'Unrestricted: no host firewall applies to this network. Ticking a network turns the firewall on.'
                    : sheetNet.access.length === 0
                      ? 'Enforced: this network reaches nothing but itself and public networks.'
                      : 'Enforced: only the networks above (and public ones) are reachable.'}
                </p>
                {!disabled && sheetNet.access != null && (
                  <button onClick={() => setAccess(sheetName, undefined)}>Unrestricted host firewall</button>
                )}
              </div>
            )}
            {canHaveShortcut(sheetNet.type) && (
              <div className="net-access">
                <h4>DNS shortcut per target</h4>
                {validTargets(draft.networks, sheetName)
                  .filter((t) => draft.networks[t].type !== 'container')
                  .map((t) => (
                    <CheckboxField
                      key={t}
                      id={`net-${sheetName}-shortcut-${t}`}
                      label={`Shortcut to ${t}`}
                      value={shortcutOn(draft.networks, sheetName, t)}
                      onChange={(v) => setShortcut(sheetName, t, v)}
                      hint={`Clients on ${sheetName} asking for a route on ${t} get this node's address on ${sheetName}.`}
                      help={`On (default): the cluster DNS answers clients on ${sheetName} with the node's address on ${sheetName} for routes on ${t}, so traffic stays on ${sheetName}. Off: they get ${t}'s own address. A route's private_shortcut: false also turns it off.`}
                      configKey={`networks.${sheetName}.shortcut`}
                      disabled={disabled}
                    />
                  ))}
              </div>
            )}
          </>
        )}
      </BottomSheet>
    </>
  );
}

/** One network's settings: rename, type, address range, DNS, kind-specific options, remove. */
function NetworkForm({
  name,
  net,
  draft,
  setDraft,
  disabled,
  original,
  onRenamed,
  onRemoved,
}: {
  name: string;
  net: NetworkConfig;
  draft: ClusterConfig;
  setDraft: (d: ClusterConfig) => void;
  disabled: boolean;
  original: ClusterConfig;
  onRenamed: (name: string) => void;
  onRemoved: () => void;
}) {
  const [renaming, setRenaming] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState('');
  const [renameError, setRenameError] = useState<string | null>(null);
  const [renameBusy, setRenameBusy] = useState(false);

  const updateNetwork = (n: string, patch: Partial<NetworkConfig>) => {
    setDraft({
      ...draft,
      networks: {
        ...draft.networks,
        [n]: { ...draft.networks[n], ...patch },
      },
    });
  };

  const removeNetwork = (n: string) => {
    const next = { ...draft.networks };
    delete next[n];
    const dns = draft.proxy?.dns_server;
    const proxy =
      dns && dns.networks.includes(n)
        ? { ...draft.proxy, dns_server: { ...dns, networks: removeDnsNetwork(dns.networks, n) } }
        : draft.proxy;
    setDraft({ ...draft, networks: next, proxy });
    onRemoved();
  };

  const startRename = (n: string) => {
    setRenaming(n);
    setRenameValue(n);
    setRenameError(null);
  };

  const finishRename = (oldName: string, newName: string) => {
    setDraft(renameNetworkInDraft(draft, oldName, newName));
    setRenaming(null);
    onRenamed(newName);
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
      finishRename(oldName, newName);
      return;
    }
    setRenameBusy(true);
    setRenameError(null);
    try {
      await api.renameNetwork(oldName, newName);
      finishRename(oldName, newName);
    } catch (e) {
      setRenameError(e instanceof ApiError ? e.message : 'rename failed');
    } finally {
      setRenameBusy(false);
    }
  };

  return (
        <div className="net-form">
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
            label="Network type"
            value={net.type}
            onChange={(type) => {
              const kind = type as NetworkKind;
              if (kind === net.type) return;
              const next = { ...draft.networks[name], type: kind };
              if (kind !== 'private') delete next.managed;
              if (kind === 'container') next.container = emptyContainer();
              else delete next.container;
              setDraft({ ...draft, networks: { ...draft.networks, [name]: next } });
            }}
            options={[
              { value: 'internal', label: 'Internal' },
              { value: 'private', label: 'Private' },
              { value: 'public', label: 'Public' },
              { value: 'egress', label: 'Egress' },
              { value: 'wireguard', label: 'WireGuard' },
              { value: 'nat', label: 'NAT' },
              { value: 'container', label: 'Container' },
            ]}
            hint="What this network is used for."
            help="Internal carries raft, drive sync and the container overlay, and is required exactly once. Private is a subnetted network for other trusted traffic. Public has no subnet; nodes get an IPv4/IPv6/domain address block instead. Egress routes a service's outbound traffic out a named host interface instead of the node's normal route; pick that interface per node below. WireGuard is configured per node by importing a wg-quick config below. NAT routes a service's outbound traffic through the node's own routing table, masqueraded like ordinary NAT, instead of a named interface — no per-node setup needed. Container routes an address space through a router container in a stack (a VPN or tunnel client, for example); stacks listing the network and the chosen hosts send traffic for that range through it."
            configKey={`networks.${name}.type`}
            disabled={disabled}
          />
          {net.type !== 'public' && net.type !== 'wireguard' && net.type !== 'egress' && net.type !== 'nat' && (
            <CidrField
              id={`net-${name}-subnet`}
              label={net.type === 'container' ? 'Routed address range' : 'Address range'}
              value={net.subnet ?? ''}
              onChange={(subnet) => updateNetwork(name, { subnet: subnet || null })}
              hint={net.type === 'container' ? 'CIDR reached through the router, e.g. 100.64.0.0/10. 0.0.0.0/0 routes everything.' : 'CIDR, e.g. 10.10.0.0/24.'}
              help={net.type === 'container' ? 'The address space behind the router. Stacks listing this network, and the hosts below, send traffic for it through the router container.' : 'The address range nodes on this network get their addresses from.'}
              configKey={`networks.${name}.subnet`}
              disabled={disabled}
            />
          )}
          {net.type === 'private' && (
            <>
              <CheckboxField
                id={`net-${name}-managed`}
                label="Managed (VXLAN)"
                value={net.managed === true}
                onChange={(v) => updateNetwork(name, { managed: v ? true : undefined })}
                hint="Clouduler builds the overlay and hands out container addresses. Needs an IPv4 subnet."
                help="Clouduler builds one cluster-wide VXLAN overlay over the address range, between the nodes that list this network. Containers talk to each other over it and keep their addresses from the bottom of the range; the first host address is the shared gateway. Every node that lists the network gets a node address on it (automatic from the top of the range, or set explicitly on the node). Outside clients never reach container addresses and those are never published in DNS: they reach the services through the node addresses, via the open ports below or the reverse proxy. Your router must route the subnet to the nodes; clouduler admits a source only when its own network lists this one in 'Can reach'. Internet access for the containers needs a separate nat, egress or wireguard network. The flag and the subnet cannot be changed while a deployed stack uses the network."
                configKey={`networks.${name}.managed`}
                disabled={disabled}
              />
              {net.managed === true && (() => {
                const err = managedNetworkError(draft, name);
                return err ? (
                  <p className="field-error">
                    <Icon name="alert-circle" /> {err}
                  </p>
                ) : null;
              })()}
              {net.managed === true && (
                <>
                  <CheckboxField
                    id={`net-${name}-proxy`}
                    label="Reverse proxy"
                    value={net.proxy === true}
                    onChange={(v) => updateNetwork(name, { proxy: v ? true : undefined })}
                    hint="Serve 80/443 for this network's hostnames on any node."
                    help="The reverse proxy answers 80 and 443 for the hostnames of stacks on this network on every node that lists it, relaying to the node holding the container (preferring that node). DNS for those hostnames is published for this network instead of following the global proxy DNS publish mode. Without it, only the open ports below are reachable, on the owning node's address."
                    configKey={`networks.${name}.proxy`}
                    disabled={disabled}
                  />
                  <ManagedOpenPortsEditor
                    idPrefix={`net-${name}`}
                    ports={net.open_ports ?? []}
                    onChange={(open_ports) => updateNetwork(name, { open_ports: open_ports.length > 0 ? open_ports : undefined })}
                    proxy={net.proxy === true}
                    disabled={disabled}
                  />
                </>
              )}
            </>
          )}
          {net.type !== 'wireguard' && net.type !== 'egress' && net.type !== 'nat' && (
            <TextField
              id={`net-${name}-domain`}
              label="DNS domain"
              value={net.domain ?? ''}
              onChange={(domain) => updateNetwork(name, { domain: domain || null })}
              hint="Names under it resolve to node addresses here."
              help="Records under this domain resolve to node addresses on this network."
              configKey={`networks.${name}.domain`}
              disabled={disabled}
            />
          )}
          {(net.type === 'internal' || net.type === 'private') && (
            <CheckboxField
              id={`net-${name}-publish-dns`}
              label="Publish to DNS provider (Cloudflare)"
              value={net.publish_dns ?? true}
              onChange={(v) => updateNetwork(name, { publish_dns: v ? undefined : false })}
              hint="Turn off to keep this network's hostnames out of Cloudflare; they are then answered only by the cluster DNS server."
              help="Turn off to keep this network's hostnames out of Cloudflare; they are then answered only by the cluster DNS server."
              configKey={`networks.${name}.publish_dns`}
              disabled={disabled}
            />
          )}
          {canServeDns(net) && (() => {
            const dns = draft.proxy?.dns_server;
            const list = dns?.networks ?? [];
            const off = !draft.proxy?.enabled || !dns?.enabled;
            const noRouter = net.type === 'container' && net.container?.proxy !== true;
            const on = !noRouter && isServingDns(draft.networks, list, name);
            const last = on && isLastServed(draft.networks, list, name);
            const hint = noRouter
              ? 'Needs the router side to reach the proxy (proxy: true).'
              : off
                ? 'The cluster DNS server is off; enable it under Reverse proxy.'
                : last
                  ? "The node's address on this network answers DNS queries. At least one network must stay served."
                  : "The node's address on this network answers DNS queries.";
            return (
              <CheckboxField
                id={`net-${name}-dns-server`}
                label="Serve cluster DNS on this network"
                value={on}
                onChange={(v) => {
                  const next = toggleDnsNetwork(draft.networks, list, name, v);
                  if (next === null || !dns) return;
                  setDraft({ ...draft, proxy: { ...draft.proxy, dns_server: { ...dns, networks: next } } });
                }}
                hint={hint}
                help="The cluster DNS server listens on the node's address on this network and answers cluster hostnames. Leaving every network at its default serves all internal and private networks (and container networks with proxy: true); a public network is served only when switched on."
                configKey="proxy.dns_server.networks"
                disabled={disabled || off || noRouter || last}
              />
            );
          })()}
          {net.type === 'wireguard' && (
            <p className="field-hint">
              Not addressable: import each node's WireGuard config below, under its own card.
            </p>
          )}
          {net.type === 'egress' && (
            <p className="field-hint">
              Not addressable by subnet: pick a host interface per node below. Clouduler does not manage the interface
              itself — it must already exist and be up.
            </p>
          )}
          {net.type === 'container' && (
            <ContainerNetworkForm
              name={name}
              value={net.container ?? emptyContainer()}
              onChange={(container) => updateNetwork(name, { container })}
              draft={draft}
              disabled={disabled}
            />
          )}
          {net.type === 'nat' && (
            <p className="field-hint">
              Not addressable, and nothing to configure per node: every node implements it the same way, NAT-ing a
              service's outbound traffic out whatever interface its own routing table picks, instead of a single fixed
              interface.
            </p>
          )}
        </div>
  );
}
