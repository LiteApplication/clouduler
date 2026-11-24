import { useState } from 'react';
import type { DnsServerConfig } from '../../../api/types';
import { CheckboxField, PortField, TextField } from '../../../components/FormFields';
import type { SectionProps } from './types';

const DEFAULT_DNS_SERVER: DnsServerConfig = {
  enabled: false,
  networks: [],
  port: 53,
  upstream: [],
};

/** Splits a comma or space separated list of resolver addresses. */
export function parseUpstreamList(text: string): string[] {
  return text.split(/[\s,]+/).filter(Boolean);
}

function DnsUpstreamField({
  value,
  onChange,
  disabled,
}: {
  value: string[];
  onChange: (upstream: string[]) => void;
  disabled: boolean;
}) {
  // Local text, so a trailing comma or space survives while typing.
  const [text, setText] = useState(value.join(', '));
  return (
    <TextField
      id="proxy-dns-server-upstream"
      label="DNS server upstream"
      configKey="proxy.dns_server.upstream"
      value={text}
      onChange={(next) => {
        setText(next);
        onChange(parseUpstreamList(next));
      }}
      hint="Resolver IPs, comma separated. Blank uses the host's resolvers without Tailscale MagicDNS."
      help="Resolvers that names the cluster does not own are forwarded to, tried in order on port 53. Leave blank to use the host's own resolvers except Tailscale MagicDNS (100.100.100.100), which split DNS can route straight back to this server and loop; if MagicDNS is the host's only resolver, 1.1.1.1 and 1.0.0.1 are used. Applies live."
      disabled={disabled}
    />
  );
}

export function DnsServerFields({ draft, setDraft, disabled }: Pick<SectionProps, 'draft' | 'setDraft' | 'disabled'>) {
  const d = draft.proxy.dns_server ?? DEFAULT_DNS_SERVER;
  const set = (patch: Partial<DnsServerConfig>) =>
    setDraft({
      ...draft,
      proxy: { ...draft.proxy, dns_server: { ...d, ...patch } },
    });
  return (
    <>
      <CheckboxField
        id="proxy-dns-server-enabled"
        label="Run a cluster DNS server"
        value={d.enabled}
        onChange={(enabled) => set({ enabled })}
        hint="Answers cluster hostnames by the network the client is on."
        help="Answers cluster hostnames by the network the client is on: a client gets the node's address on its own network; hostnames on networks it can't reach are refused; other names are forwarded upstream for private-network clients only."
        configKey="proxy.dns_server.enabled"
        disabled={disabled}
      />
      {d.enabled && (
        <>
          <PortField
            id="proxy-dns-server-port"
            label="DNS server port"
            value={d.port}
            onChange={(port) => set({ port })}
            hint="Usually 53."
            help="UDP/TCP port the cluster DNS server listens on. Below 1024, so the daemon needs root to bind it."
            configKey="proxy.dns_server.port"
            disabled={disabled}
          />
          <DnsUpstreamField value={d.upstream ?? []} onChange={(upstream) => set({ upstream })} disabled={disabled} />
        </>
      )}
    </>
  );
}
