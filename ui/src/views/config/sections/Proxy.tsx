import { CheckboxField, PortField, TextField } from '../../../components/FormFields';
import type { SectionProps } from './types';
import { DnsServerFields } from './Dns';

export function ProxySection({ draft, setDraft, disabled }: SectionProps) {
  const p = draft.proxy;
  const set = (patch: Partial<typeof p>) => setDraft({ ...draft, proxy: { ...p, ...patch } });
  return (
    <div className="card">
      <CheckboxField
        id="proxy-enabled"
        label="Run the reverse proxy"
        value={p.enabled}
        onChange={(enabled) => set({ enabled })}
        hint="Serves every route your stacks declare."
        help="Terminates HTTP/HTTPS for every service route declared under a stack's x-clouduler.proxy and forwards to its container. Both ports below are bound on every node, on every address, since a route follows its container as it moves."
        configKey="proxy.enabled"
        disabled={disabled}
      />
      <PortField
        id="proxy-http-port"
        label="HTTP port"
        value={p.http_port}
        onChange={(http_port) => set({ http_port })}
        hint="Usually 80."
        help="Usually 80. Below 1024, so the daemon needs root to bind it."
        configKey="proxy.http_port"
        disabled={disabled || !p.enabled}
      />
      <PortField
        id="proxy-https-port"
        label="HTTPS port"
        value={p.https_port}
        onChange={(https_port) => set({ https_port })}
        hint="Usually 443."
        help="Must differ from the web ui's port, which listens on the same addresses."
        configKey="proxy.https_port"
        disabled={disabled || !p.enabled}
      />
      <CheckboxField
        id="proxy-acme-enabled"
        label="Issue certificates automatically"
        value={p.acme.enabled}
        onChange={(enabled) => set({ acme: { ...p.acme, enabled } })}
        hint="Needs Cloudflare DNS below."
        help="Automatically obtains and renews TLS certificates via ACME DNS-01 for every published route's hostname. Requires Cloudflare DNS management to be configured below, as DNS-01 is the only challenge type supported."
        configKey="proxy.acme.enabled"
        disabled={disabled || !p.enabled}
      />
      {p.acme.enabled && (
        <>
          <TextField
            id="proxy-acme-directory"
            label="ACME directory URL"
            value={p.acme.directory_url ?? ''}
            onChange={(v) => set({ acme: { ...p.acme, directory_url: v || null } })}
            hint="Leave blank for Let's Encrypt production."
            configKey="proxy.acme.directory_url"
            disabled={disabled}
          />
          <TextField
            id="proxy-acme-contact"
            label="Contact email"
            value={p.acme.contact_email ?? ''}
            onChange={(v) => set({ acme: { ...p.acme, contact_email: v || null } })}
            hint="Where the CA sends expiry warnings."
            configKey="proxy.acme.contact_email"
            disabled={disabled}
          />
        </>
      )}
      <DnsServerFields draft={draft} setDraft={setDraft} disabled={disabled} />
      <p className="field-hint">
        Static/external backends and redirects apply immediately from the Redirects page, since they live outside
        cluster.yaml. See the{' '}
        <a href="https://clouduler.liteapp.fr/docs/features/proxy-and-tls" target="_blank" rel="noreferrer">
          proxy documentation
        </a>
        .
      </p>
    </div>
  );
}
