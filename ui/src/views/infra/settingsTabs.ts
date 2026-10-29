export const SETTINGS_TABS = [
  { id: 'general', label: 'General' },
  { id: 'network', label: 'Network' },
  { id: 'storage', label: 'Storage' },
  { id: 'security', label: 'Security' },
  { id: 'secrets', label: 'Secrets & configs' },
  { id: 'access', label: 'Access' },
  { id: 'self-update', label: 'Self-update' },
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
  secrets: 'secrets',
};

