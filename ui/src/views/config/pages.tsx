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
  GitBackupSection,
  McpSection,
  NetworksSection,
  ProxySection,
  SecretsSection,
  WebSection,
  type SectionProps,
} from './sections';

/** Groups the sidebar submenu and the review table organize pages under. */
export type ConfigGroup = 'General' | 'Network & Access' | 'Storage' | 'Security';

export const CONFIG_GROUPS: ConfigGroup[] = ['General', 'Network & Access', 'Storage', 'Security'];

export interface ConfigPage {
  slug: string;
  label: string;
  icon: IconName;
  blurb: string;
  group: ConfigGroup;
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
    group: 'General',
    keys: ['cluster'],
    Component: ClusterSection,
  },
  {
    slug: 'failover',
    label: 'Failover',
    icon: 'activity',
    blurb: 'Timers deciding when a node is considered gone and its work moves.',
    group: 'General',
    keys: ['failover'],
    Component: FailoverSection,
  },
  {
    slug: 'networks',
    label: 'Networks',
    icon: 'network',
    blurb: 'The internal overlay plus any private or public networks nodes are reachable on.',
    group: 'Network & Access',
    keys: ['networks'],
    Component: NetworksSection,
  },
  {
    slug: 'web',
    label: 'Web UI',
    icon: 'overview',
    blurb: 'Where this console listens and under which name it is published.',
    group: 'Network & Access',
    keys: ['web'],
    Component: WebSection,
  },
  {
    slug: 'proxy',
    label: 'Reverse proxy',
    icon: 'external',
    blurb: 'Published routes and the ACME certificates that front them.',
    group: 'Network & Access',
    keys: ['proxy'],
    Component: ProxySection,
  },
  {
    slug: 'cloudflare',
    label: 'Cloudflare',
    icon: 'certificate',
    blurb: 'DNS records clouduler keeps in sync with your zones.',
    group: 'Network & Access',
    keys: ['cloudflare'],
    Component: CloudflareSection,
  },
  {
    slug: 'drives',
    label: 'Drives',
    icon: 'drives',
    blurb: 'How each drive is replicated or served across the cluster.',
    group: 'Storage',
    keys: ['drives'],
    Component: DrivesSection,
  },
  {
    slug: 'auth',
    label: 'Auth',
    icon: 'lock',
    blurb: 'API keys, session lifetime and the OIDC provider.',
    group: 'Security',
    keys: ['auth'],
    Component: AuthSection,
  },
  {
    slug: 'secrets',
    label: 'Secrets',
    icon: 'key',
    blurb: 'Where the cluster key lives and whether a node may create one.',
    group: 'Security',
    keys: ['secrets'],
    Component: SecretsSection,
  },
  {
    slug: 'mcp',
    label: 'AI access',
    icon: 'sparkle',
    blurb: 'Whether AI clients may connect to the cluster over MCP.',
    group: 'Security',
    keys: ['mcp'],
    Component: McpSection,
  },
  {
    slug: 'git-backup',
    label: 'Git backup',
    icon: 'sync',
    blurb: 'Mirror cluster state into a git repository for disaster recovery.',
    group: 'Security',
    keys: ['backup'],
    Component: GitBackupSection,
  },
];

/** Page owning a given diff path, so Review can link straight to its editor.
 *  A `nodes.<name>.*` path has no page of its own because that config now lives on
 *  the node's own detail page, so it never matches here; see
 *  `nodePathTarget` for that case. */
export function pageForPath(path: string): ConfigPage | undefined {
  const top = path.split('.')[0];
  return CONFIG_PAGES.find((p) => p.keys.includes(top));
}

/** The node detail route owning a `nodes.<name>.*` diff path, if any. */
export function nodePathTarget(path: string): string | undefined {
  const match = /^nodes\.([^.]+)/.exec(path);
  return match ? `/nodes/${encodeURIComponent(match[1])}` : undefined;
}
