import { useEffect, useMemo, useState } from 'react';
import { parseDocument } from 'yaml';
import { useClusterState } from '../../../state/store';
import { api, ApiError, encodeText } from '../../../api/client';
import type { ClusterConfig, ContainerNetwork, ContainerOpenPort, ManagedOpenPort, EgressFallback, InterfaceAddress, OpenPort, PortProtocol } from '../../../api/types';
import { CheckOption, HelpTip, useConfirm } from '../../../ds';
import { Icon } from '../../../components/Icon';
import { CheckboxField, IpField, PortField, SelectField, TextField } from '../../../components/FormFields';

export function emptyContainer(): ContainerNetwork {
  return { stack: '', service: '', hosts: [], open_ports: [], proxy: false };
}

/** Service names of a stack spec (compose yaml); null when it can't be parsed. */
export function serviceNamesOf(spec: string | undefined): string[] | null {
  if (!spec) return null;
  try {
    const doc = parseDocument(spec);
    if (doc.errors.length) return null;
    const services = (doc.toJS() as { services?: Record<string, unknown> } | null)?.services;
    if (!services || typeof services !== 'object') return null;
    return Object.keys(services);
  } catch {
    return null;
  }
}

export function MultiPick({
  label,
  options,
  selected,
  onChange,
  disabled,
  empty,
}: {
  label: string;
  options: string[];
  selected: string[];
  onChange: (v: string[]) => void;
  disabled: boolean;
  empty: string;
}) {
  // Keep selections that no longer exist visible so they can be unticked.
  const all = Array.from(new Set([...options, ...selected])).sort();
  return (
    <div className="field" role="group" aria-label={label}>
      <div className="fld__head">
        <label>{label}</label>
      </div>
      {all.length === 0 && <p className="field-hint">{empty}</p>}
      {all.map((o) => (
        <CheckOption
          key={o}
          checked={selected.includes(o)}
          disabled={disabled}
          onChange={(on) => onChange(on ? [...selected, o].sort() : selected.filter((x) => x !== o))}
        >
          {o}
        </CheckOption>
      ))}
    </div>
  );
}

/** The router + routing options of a `container`-kind network. */
export function ContainerNetworkForm({
  name,
  value,
  onChange,
  draft,
  disabled,
}: {
  name: string;
  value: ContainerNetwork;
  onChange: (c: ContainerNetwork) => void;
  draft: ClusterConfig;
  disabled: boolean;
}) {
  const cluster = useClusterState();
  const stacks = cluster?.stacks ?? {};
  const stackNames = Object.keys(stacks).sort();
  const services = useMemo(() => serviceNamesOf(stacks[value.stack]?.spec), [stacks, value.stack]);
  const set = (patch: Partial<ContainerNetwork>) => onChange({ ...value, ...patch });

  return (
    <>
      <SelectField
        id={`net-${name}-container-stack`}
        label="Router stack"
        value={value.stack}
        onChange={(stack) => set({ stack, service: '' })}
        options={[
          { value: '', label: '(choose a stack)' },
          ...Array.from(new Set([...stackNames, ...(value.stack ? [value.stack] : [])])).map((s) => ({ value: s, label: s })),
        ]}
        hint="The stack running the router container."
        help="The stack containing the router service. It may be deployed after the network; until then the network just reports the router as missing."
        configKey={`networks.${name}.container.stack`}
        disabled={disabled}
      />
      {services ? (
        <SelectField
          id={`net-${name}-container-service`}
          label="Router service"
          value={value.service}
          onChange={(service) => set({ service })}
          options={[
            { value: '', label: '(choose a service)' },
            ...Array.from(new Set([...services, ...(value.service ? [value.service] : [])])).map((s) => ({ value: s, label: s })),
          ]}
          hint="Must run a single replica."
          help="The service inside the router stack that forwards the traffic. It must run a single replica."
          configKey={`networks.${name}.container.service`}
          disabled={disabled}
        />
      ) : (
        <TextField
          id={`net-${name}-container-service`}
          label="Router service"
          value={value.service}
          onChange={(service) => set({ service })}
          hint="Service name in the router stack."
          help="The service inside the router stack that forwards the traffic. It must run a single replica."
          configKey={`networks.${name}.container.service`}
          disabled={disabled}
        />
      )}
      <MultiPick
        label="Hosts routed through the router"
        options={Object.keys(draft.nodes)}
        selected={value.hosts ?? []}
        onChange={(hosts) => set({ hosts })}
        disabled={disabled}
        empty="No nodes in the cluster."
      />
      <OpenPortsEditor
        idPrefix={`net-${name}-container`}
        hint="Host ports the router side may connect to. The target port is where the traffic is forwarded (default: the same port)."
        ports={value.open_ports ?? []}
        onChange={(open_ports) => set({ open_ports })}
        disabled={disabled}
        withTarget
      />
      <CheckboxField
        id={`net-${name}-container-proxy`}
        label="Let the router reach the reverse proxy (80/443)"
        value={value.proxy ?? false}
        onChange={(proxy) => set({ proxy })}
        hint="Lets clients behind the router open proxied routes."
        help="When on, the router side can reach the reverse proxy on ports 80 and 443 of the node it connects through."
        configKey={`networks.${name}.container.proxy`}
        disabled={disabled}
      />
    </>
  );
}

/**
 * A managed network's open ports: each is exposed on the node address of the
 * node holding the container of `stack`/`service`, forwarded to `target_port`.
 */
export function ManagedOpenPortsEditor({
  idPrefix,
  ports,
  onChange,
  proxy,
  disabled,
}: {
  idPrefix: string;
  ports: ManagedOpenPort[];
  onChange: (ports: ManagedOpenPort[]) => void;
  proxy: boolean;
  disabled: boolean;
}) {
  const [adding, setAdding] = useState(false);
  const [port, setPort] = useState(0);
  const [targetPort, setTargetPort] = useState(0);
  const [protocol, setProtocol] = useState<PortProtocol>('tcp');
  const [stack, setStack] = useState('');
  const [service, setService] = useState('');
  const dup = ports.some((p) => p.port === port && p.protocol === protocol);
  const webClash = proxy && protocol === 'tcp' && (port === 80 || port === 443);
  const valid = port > 0 && stack.trim() !== '' && service.trim() !== '' && !dup && !webClash;

  const add = () => {
    if (!valid) return;
    const entry: ManagedOpenPort = {
      port,
      protocol,
      ...(targetPort && targetPort !== port ? { target_port: targetPort } : {}),
      stack: stack.trim(),
      service: service.trim(),
    };
    onChange([...ports, entry].sort((a, b) => a.port - b.port || a.protocol.localeCompare(b.protocol)));
    setPort(0);
    setTargetPort(0);
    setStack('');
    setService('');
    setAdding(false);
  };

  return (
    <div className="field">
      <div className="fld__head">
        <label>Open ports</label>
        <HelpTip label="Open ports">
          Ports exposed on the node address of the node that holds the container of the named stack and service, forwarded to the
          container (to the target port when set). Raw ports are reached through DNS pointing at that node, so a client's network
          must reach it. Nothing else of the network is reachable from outside; stopped containers expose nothing.
        </HelpTip>
      </div>
      {ports.length === 0 && <p className="field-hint">No open ports.</p>}
      {ports.map((p) => (
        <div key={`${p.protocol}-${p.port}`} className="flex-row" style={{ gap: 8, marginBottom: 4 }}>
          <span className="mono">
            {p.port}/{p.protocol}
            {p.target_port && p.target_port !== p.port ? ` → ${p.target_port}` : ''} → {p.stack}/{p.service}
          </span>
          {!disabled && (
            <button type="button" onClick={() => onChange(ports.filter((q) => q !== p))} aria-label={`remove ${p.port}/${p.protocol}`}>
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
        <div className="flex-row" style={{ gap: 8, alignItems: 'flex-end', flexWrap: 'wrap' }}>
          <PortField id={`${idPrefix}-mop-port`} label="Port" value={port} onChange={setPort} disabled={disabled} />
          <SelectField
            id={`${idPrefix}-mop-protocol`}
            label="Protocol"
            value={protocol}
            onChange={setProtocol}
            options={[
              { value: 'tcp', label: 'TCP' },
              { value: 'udp', label: 'UDP' },
            ]}
            disabled={disabled}
          />
          <PortField id={`${idPrefix}-mop-target`} label="Target port (blank = same)" value={targetPort} onChange={setTargetPort} disabled={disabled} />
          <TextField id={`${idPrefix}-mop-stack`} label="Stack" value={stack} onChange={setStack} disabled={disabled} />
          <TextField id={`${idPrefix}-mop-service`} label="Service" value={service} onChange={setService} disabled={disabled} />
          <button type="button" onClick={add} disabled={!valid}>
            <Icon name="check" /> Add
          </button>
          <button type="button" onClick={() => setAdding(false)}>
            Cancel
          </button>
        </div>
      )}
      {adding && dup && <p className="field-error">{port}/{protocol} is already listed.</p>}
      {adding && webClash && <p className="field-error">{port}/tcp is served by the reverse proxy and cannot also be an open port.</p>}
    </div>
  );
}

/** Ports a node has open on one network's address: a router forwarding them
 *  in (public) or a VPN tunnel's far end forwarding them back (wireguard). */
export function OpenPortsEditor({
  idPrefix,
  hint,
  ports,
  onChange,
  disabled,
  withTarget = false,
}: {
  idPrefix: string;
  hint: string;
  ports: ContainerOpenPort[];
  onChange: (ports: ContainerOpenPort[]) => void;
  disabled: boolean;
  /** Also edit `target_port` (the host port the traffic is forwarded to). */
  withTarget?: boolean;
}) {
  const [adding, setAdding] = useState(false);
  const [port, setPort] = useState(0);
  const [targetPort, setTargetPort] = useState(0);
  const [protocol, setProtocol] = useState<PortProtocol>('tcp');

  const add = () => {
    if (!port || ports.some((p) => p.port === port && p.protocol === protocol)) return;
    const entry: ContainerOpenPort = withTarget && targetPort && targetPort !== port ? { port, protocol, target_port: targetPort } : { port, protocol };
    onChange([...ports, entry].sort((a, b) => a.port - b.port || a.protocol.localeCompare(b.protocol)));
    setPort(0);
    setTargetPort(0);
    setAdding(false);
  };

  const remove = (target: OpenPort) => {
    onChange(ports.filter((p) => !(p.port === target.port && p.protocol === target.protocol)));
  };

  return (
    <div className="field">
      <div className="fld__head">
        <label>Open ports</label>
        <HelpTip label="Open ports">{hint}</HelpTip>
      </div>
      {ports.map((p) => (
        <div key={`${p.protocol}-${p.port}`} className="flex-row" style={{ gap: 8, marginBottom: 4 }}>
          <span className="mono">
            {p.port}/{p.protocol}
            {withTarget && p.target_port && p.target_port !== p.port ? ` → ${p.target_port}` : ''}
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
          {withTarget && (
            <PortField
              id={`${idPrefix}-open-port-target`}
              label="Target port (blank = same)"
              value={targetPort}
              onChange={setTargetPort}
              disabled={disabled}
            />
          )}
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
export function EgressInterfaceField({
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
    onChange({
      ...addr,
      fallback: enabled ? { interface: '', health_check_ip: '' } : null,
    });
  };

  const updateFallback = (patch: Partial<EgressFallback>) => {
    onChange({
      ...addr,
      fallback: {
        ...(addr.fallback ?? { interface: '', health_check_ip: '' }),
        ...patch,
      },
    });
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
        <div className="fld__head">
          <label htmlFor={`node-${nodeName}-${netName}-iface`}>Interface</label>
          <HelpTip label="Interface">
            The host interface this node's outbound traffic on this network leaves through, e.g. a VPN tunnel or the
            node's WAN interface. Must already exist on the node; clouduler does not bring it up.
          </HelpTip>
        </div>
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
      </div>
      <CheckboxField
        id={`node-${nodeName}-${netName}-fallback-enabled`}
        label="Use a backup interface"
        value={fallbackEnabled}
        onChange={toggleFallback}
        hint="Takes over when the main one stops answering."
        help="If this node cannot reach an IP through the interface above for one reconcile tick, route this network out a second interface instead until the first one answers again."
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
            label="Health check address"
            value={addr.fallback?.health_check_ip ?? ''}
            onChange={(v) => updateFallback({ health_check_ip: v })}
            hint="Pinged every tick, e.g. 1.1.1.1."
            help="Checked by pinging it out the primary interface, once per reconcile tick. Pick something reliably up, e.g. 1.1.1.1."
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
export function WireguardAddressField({
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
  const confirm = useConfirm();
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
    const ok = await confirm({
      title: `Remove the WireGuard config for ${nodeName}?`,
      body: `${nodeName} stops using its imported tunnel config on ${netName}.`,
      confirmLabel: 'Remove config',
      confirmVariant: 'danger',
    });
    if (!ok) return;
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
          placeholder={
            '[Interface]\nPrivateKey = ...\nAddress = ...\n\n[Peer]\nPublicKey = ...\nEndpoint = ...\nAllowedIPs = 0.0.0.0/0'
          }
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
