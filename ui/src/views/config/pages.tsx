// The configuration subpages, listed once and used twice: the sidebar renders
// them as a submenu under Configuration, and the Configuration shell mounts
// them as routes.

import { type IconName } from '../../components/Icon';
import {
  AuthSection,
  ServiceLoginSection,
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
    blurb: 'The name shown across the UI.',
    group: 'General',
    keys: ['cluster'],
    Component: ClusterSection,
  },
  {
    slug: 'failover',
    label: 'Failover',
    icon: 'activity',
    blurb: 'How fast a silent node is declared lost and its work moves.',
    group: 'General',
    keys: ['failover'],
    Component: FailoverSection,
  },
  {
    slug: 'networks',
    label: 'Networks',
    icon: 'network',
    blurb: 'The internal overlay plus private, public and VPN networks.',
    group: 'Network & Access',
    keys: ['networks'],
    Component: NetworksSection,
  },
  {
    slug: 'web',
    label: 'Dashboard',
    icon: 'overview',
    blurb: 'Where this console listens and the name it is reached by.',
    group: 'Network & Access',
    keys: ['web'],
    Component: WebSection,
  },
  {
    slug: 'proxy',
    label: 'Reverse proxy',
    icon: 'external',
    blurb: 'Publishes your stacks\' web routes over HTTPS.',
    group: 'Network & Access',
    keys: ['proxy'],
    Component: ProxySection,
  },
  {
    slug: 'cloudflare',
    label: 'Cloudflare DNS',
    icon: 'certificate',
    blurb: 'Keeps DNS records in sync with your zones.',
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
    label: 'Sign-in',
    icon: 'lock',
    blurb: 'API keys, session length and single sign-on.',
    group: 'Security',
    keys: ['auth'],
    Component: AuthSection,
  },
  {
    slug: 'service-login',
    label: 'Service login',
    icon: 'lock',
    blurb: 'Log visitors of your apps in, for routes with the authenticated middleware.',
    group: 'Security',
    keys: ['proxy.auth'],
    Component: ServiceLoginSection,
  },
  {
    slug: 'secrets',
    label: 'Cluster key',
    icon: 'key',
    blurb: 'The key that seals secrets, and who may create it.',
    group: 'Security',
    keys: ['secrets'],
    Component: SecretsSection,
  },
  {
    slug: 'mcp',
    label: 'AI access',
    icon: 'sparkle',
    blurb: 'Let tools like Claude Code connect over MCP.',
    group: 'Security',
    keys: ['mcp'],
    Component: McpSection,
  },
  {
    slug: 'git-backup',
    label: 'Git backup',
    icon: 'sync',
    blurb: 'An encrypted copy of cluster state in a git repo.',
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
  // Longest key wins, so `proxy.auth.*` lands on Service login, not Proxy.
  let best: ConfigPage | undefined;
  let bestLen = -1;
  for (const p of CONFIG_PAGES) {
    for (const k of p.keys) {
      if ((path === k || path.startsWith(`${k}.`)) && k.length > bestLen) {
        best = p;
        bestLen = k.length;
      }
    }
  }
  return best;
}

/** The node detail route owning a `nodes.<name>.*` diff path, if any. */
export function nodePathTarget(path: string): string | undefined {
  const match = /^nodes\.([^.]+)/.exec(path);
  return match ? `/nodes/${encodeURIComponent(match[1])}` : undefined;
}
