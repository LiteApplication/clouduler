import type { IconName } from '../../ds';

export interface SettingsTab {
  /** The /settings/<id> slug. Slugs are kept stable so old links keep working. */
  id: string;
  label: string;
  /** One line under the label in the menu. */
  desc: string;
  /** The page's own subtitle. */
  sub: string;
  icon: IconName;
  group: string;
}

/** The menu's groups, in order. */
export const SETTINGS_GROUPS = ['Cluster', 'Connectivity', 'Data', 'Security'];

export const SETTINGS_TABS: SettingsTab[] = [
  {
    id: 'general',
    label: 'General',
    desc: 'Cluster name and failover timers',
    sub: 'The cluster itself, and how quickly a silent node is declared lost.',
    icon: 'settings',
    group: 'Cluster',
  },
  {
    id: 'self-update',
    label: 'Updates',
    desc: 'Roll out a new release',
    sub: 'Move every node to a new Clouduler release, one at a time.',
    icon: 'download',
    group: 'Cluster',
  },
  {
    id: 'network',
    label: 'Network',
    desc: 'Networks, dashboard, proxy, DNS',
    sub: 'How nodes reach each other and the world. Risky changes roll back unless you confirm.',
    icon: 'network',
    group: 'Connectivity',
  },
  {
    id: 'storage',
    label: 'Storage',
    desc: 'How drives are replicated',
    sub: 'Whether each drive is copied to every node or served from one.',
    icon: 'drives',
    group: 'Data',
  },
  {
    id: 'secrets',
    label: 'Secrets and configs',
    desc: 'Sealed values and config files',
    sub: 'Values stacks can use. Secret values are never shown.',
    icon: 'secrets',
    group: 'Data',
  },
  {
    id: 'security',
    label: 'Sign-in and backup',
    desc: 'Login, OIDC, AI access, git backup',
    sub: 'Who can sign in, whether AI tools may connect, and the encrypted git backup.',
    icon: 'lock',
    group: 'Security',
  },
  {
    id: 'access',
    label: 'API keys and AI clients',
    desc: 'Keys and approved tools',
    sub: 'What is already allowed to reach this cluster.',
    icon: 'key',
    group: 'Security',
  },
];

/** Which settings tab owns an old /configuration/<slug> page. */
export const CONFIG_SLUG_TAB: Record<string, string> = {
  cluster: 'general',
  failover: 'general',
  review: 'general',
  networks: 'network',
  web: 'network',
  proxy: 'network',
  cloudflare: 'network',
  drives: 'storage',
  auth: 'security',
  mcp: 'security',
  'git-backup': 'security',
  secrets: 'security',
};
