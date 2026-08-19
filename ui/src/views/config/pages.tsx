// The configuration subpages, listed once and used twice: the sidebar renders
// them as a submenu under Configuration, and the Configuration shell mounts
// them as routes.

import { type IconName } from '../../components/Icon';
import {
  AuthSection,
  CloudflareSection,
  ClusterSection,
  DrivesSection,
  FailoverSection,
  NetworksSection,
  NodesSection,
  ProxySection,
  SecretsSection,
  WebSection,
  type SectionProps,
} from './sections';

export interface ConfigPage {
  slug: string;
  label: string;
  icon: IconName;
  blurb: string;
  /** Top-level config keys this page owns, used for its unsaved-change count. */
  keys: string[];
  Component: (props: SectionProps) => JSX.Element;
}

export const CONFIG_PAGES: ConfigPage[] = [
  {
    slug: 'cluster',
    label: 'Cluster',
    icon: 'cloud',
    blurb: 'Identity of the cluster itself.',
    keys: ['cluster'],
    Component: ClusterSection,
  },
  {
    slug: 'networks',
    label: 'Networks',
    icon: 'network',
    blurb: 'The internal overlay plus any private or public networks nodes are reachable on.',
    keys: ['networks'],
    Component: NetworksSection,
  },
  {
    slug: 'nodes',
    label: 'Nodes',
    icon: 'nodes',
    blurb: 'Per-node role, ports, addresses and drive mounts.',
    keys: ['nodes'],
    Component: NodesSection,
  },
  {
    slug: 'drives',
    label: 'Drives',
    icon: 'drives',
    blurb: 'How each drive is replicated or served across the cluster.',
    keys: ['drives'],
    Component: DrivesSection,
  },
  {
    slug: 'web',
    label: 'Web UI',
    icon: 'overview',
    blurb: 'Where this console listens and under which name it is published.',
    keys: ['web'],
    Component: WebSection,
  },
  {
    slug: 'proxy',
    label: 'Reverse proxy',
    icon: 'external',
    blurb: 'Published routes and the ACME certificates that front them.',
    keys: ['proxy'],
    Component: ProxySection,
  },
  {
    slug: 'failover',
    label: 'Failover',
    icon: 'activity',
    blurb: 'Timers deciding when a node is considered gone and its work moves.',
    keys: ['failover'],
    Component: FailoverSection,
  },
  {
    slug: 'secrets',
    label: 'Secrets',
    icon: 'key',
    blurb: 'Where the cluster key lives and whether a node may create one.',
    keys: ['secrets'],
    Component: SecretsSection,
  },
  {
    slug: 'auth',
    label: 'Auth',
    icon: 'lock',
    blurb: 'API keys, session lifetime and the OIDC provider.',
    keys: ['auth'],
    Component: AuthSection,
  },
  {
    slug: 'cloudflare',
    label: 'Cloudflare',
    icon: 'certificate',
    blurb: 'DNS records clouduler keeps in sync with your zones.',
    keys: ['cloudflare'],
    Component: CloudflareSection,
  },
];

/** Page owning a given diff path, so Review can link straight to its editor. */
export function pageForPath(path: string): ConfigPage | undefined {
  const top = path.split('.')[0];
  return CONFIG_PAGES.find((p) => p.keys.includes(top));
}
