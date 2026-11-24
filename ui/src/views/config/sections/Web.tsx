import { Icon } from '../../../components/Icon';
import { CheckboxField, PortField, SelectField, TextField } from '../../../components/FormFields';
import { managedPortalError } from '../../../lib/managedNetwork';
import type { SectionProps } from './types';

export function WebSection({ draft, setDraft, disabled }: SectionProps) {
  return (
    <div className="card">
      <TextField
        id="web-address"
        label="Dashboard address"
        value={draft.web.address}
        onChange={(address) => setDraft({ ...draft, web: { ...draft.web, address } })}
        hint="A bare label gets the network's domain appended."
        configKey="web.address"
        disabled={disabled}
      />
      <SelectField
        id="web-network"
        label="Address network"
        value={draft.web.network}
        onChange={(network) => setDraft({ ...draft, web: { ...draft.web, network } })}
        options={Object.keys(draft.networks).map((n) => ({
          value: n,
          label: n,
        }))}
        hint="Which node address DNS points at."
        help="Picks which node address the dashboard's DNS record points at, and whose domain the Address label is qualified with. Doesn't restrict which hostname you can use. A managed network works when at least one node lists it: only those nodes answer, using their node address there, so it opens from clients whose network may reach the managed network."
        configKey="web.network"
        disabled={disabled}
      />
      {managedPortalError(draft, draft.web.network, 'the dashboard') && (
        <span className="field-error" role="alert">
          <Icon name="alert-circle" /> {managedPortalError(draft, draft.web.network, 'the dashboard')}
        </span>
      )}
      <PortField
        id="web-port"
        label="Direct HTTPS port"
        value={draft.web.port}
        onChange={(port) => setDraft({ ...draft, web: { ...draft.web, port } })}
        hint="Always on, even behind the proxy."
        help="Always listening, even when the dashboard is also published through the proxy. This is the way back in if the proxy cannot bind its ports."
        configKey="web.port"
        disabled={disabled}
      />
      <CheckboxField
        id="web-through-proxy"
        label="Also publish through the proxy"
        value={draft.web.through_proxy}
        onChange={(through_proxy) => setDraft({ ...draft, web: { ...draft.web, through_proxy } })}
        hint="Standard HTTPS port, same certificate."
        help="Serves it on the standard HTTPS port with the same Let's Encrypt certificate and Cloudflare record as any other route."
        configKey="web.through_proxy"
        disabled={disabled || !draft.proxy.enabled}
      />
      {draft.web.through_proxy && (
        <PortField
          id="web-backend-port"
          label="Proxy backend port"
          value={draft.web.backend_port}
          onChange={(backend_port) => setDraft({ ...draft, web: { ...draft.web, backend_port } })}
          hint="Loopback only; must be unique."
          help="Loopback only, plain HTTP, reachable only by this node's proxy. Must differ from every other port here."
          configKey="web.backend_port"
          disabled={disabled}
        />
      )}
    </div>
  );
}
