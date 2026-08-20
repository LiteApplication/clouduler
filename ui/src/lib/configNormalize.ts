// The API leaves unset optional fields out of the config JSON entirely
// (serde's skip_serializing_if), so a cluster that has never configured OIDC
// or Cloudflare gets a body with those keys simply absent. `undefined` is not
// `null`, which is what made "Use OIDC" and "Manage DNS through Cloudflare"
// render ticked while the settings they gate stayed hidden.
//
// Filling every optional key in as an explicit null on load fixes that once,
// for the whole form, instead of each section having to remember the
// distinction. It also keeps the diff against the running config honest:
// original and draft are normalized the same way, so a key the server omitted
// never reads as a staged change.

import type { ClusterConfig, NodeAddress } from '../api/types';

function nullable<T>(v: T | null | undefined): T | null {
  return v ?? null;
}

function normalizeAddress(addr: NodeAddress): NodeAddress {
  if (typeof addr === 'string') return addr;
  if ('interface' in addr) {
    return { interface: addr.interface, open_ports: addr.open_ports ?? [] };
  }
  return {
    ipv4: nullable(addr.ipv4),
    ipv6: nullable(addr.ipv6),
    domain: nullable(addr.domain),
    open_ports: addr.open_ports ?? [],
  };
}

function mapValues<T, U>(obj: Record<string, T>, f: (v: T) => U): Record<string, U> {
  return Object.fromEntries(Object.entries(obj ?? {}).map(([k, v]) => [k, f(v)]));
}

export function normalizeConfig(cfg: ClusterConfig): ClusterConfig {
  return {
    ...cfg,
    // Both have serde defaults, so the api always sends them. A config stored
    // before they existed still round trips through here on the way to the
    // form, and a checkbox bound to `undefined` renders unticked and then
    // stages a change nobody asked for.
    web: {
      ...cfg.web,
      through_proxy: cfg.web.through_proxy ?? true,
      backend_port: cfg.web.backend_port ?? 8444,
    },
    networks: mapValues(cfg.networks, (net) => ({
      ...net,
      subnet: nullable(net.subnet),
      domain: nullable(net.domain),
    })),
    drives: mapValues(cfg.drives, (drive) => ({
      ...drive,
      served_by: drive.served_by ?? [],
    })),
    nodes: mapValues(cfg.nodes, (node) => ({
      ...node,
      network: mapValues(node.network ?? {}, normalizeAddress),
      drives: node.drives ?? {},
      data_dir: nullable(node.data_dir),
      generate_secret_key: nullable(node.generate_secret_key),
    })),
    auth: {
      ...cfg.auth,
      oidc: cfg.auth.oidc
        ? {
            ...cfg.auth.oidc,
            client_secret_file: nullable(cfg.auth.oidc.client_secret_file),
            admin_value: nullable(cfg.auth.oidc.admin_value),
          }
        : null,
    },
    cloudflare: cfg.cloudflare ? { ...cfg.cloudflare } : null,
    proxy: {
      ...cfg.proxy,
      acme: {
        ...cfg.proxy.acme,
        directory_url: nullable(cfg.proxy.acme.directory_url),
        contact_email: nullable(cfg.proxy.acme.contact_email),
      },
    },
  };
}
