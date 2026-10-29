// App shell. From 840px a side rail carries every destination; below that the
// page gets a bottom nav (Overview, Stacks, Nodes, Logs, More) and `/more`
// holds the rest. Both read one route table so they cannot drift apart.

import { useEffect, useMemo, useState, type ReactNode } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';
import { useClusterState, useClusterStore } from '../state/store';
import { failedStacks, useInstances } from '../state/instancesStore';
import { useAuth } from '../auth/AuthContext';
import { api } from '../api/client';
import { BottomNav, SideRail, Tag, type BottomNavItem, type IconName, type SideRailEntry } from '../ds';
import { Icon } from '../ds/Icon';
import { useTheme, type ThemeChoice } from '../theme';

const THEMES: { value: ThemeChoice; icon: IconName; label: string }[] = [
  { value: 'light', icon: 'sun', label: 'Light theme' },
  { value: 'dark', icon: 'moon', label: 'Dark theme' },
  { value: 'system', icon: 'system', label: 'Follow system theme' },
];

/** The rail item that owns a path: the longest matching prefix. */
function activeFor(pathname: string, ids: string[]): string | undefined {
  let best: string | undefined;
  for (const id of ids) {
    const hit = id === '/' ? pathname === '/' : pathname === id || pathname.startsWith(`${id}/`);
    if (hit && (!best || id.length > best.length)) best = id;
  }
  return best;
}

/** Counts shown as badges: the things that need a person's attention. */
export function useNavBadges() {
  const cluster = useClusterState();
  const instances = useInstances();
  return useMemo(() => {
    const failedUpdates = Object.values(cluster?.instance_updates ?? {}).filter((u) => u.phase === 'failed').length;
    const nodeProblems = (cluster?.unreachable.length ?? 0) +
      Object.values(cluster?.nodes ?? {}).filter((n) => n.availability === 'drain').length;
    return { stacks: failedStacks(instances).size, nodes: nodeProblems, updates: failedUpdates };
  }, [cluster, instances]);
}

export function Layout({ children }: { children: ReactNode }) {
  const { status } = useClusterStore();
  const { identity, isAdmin, refresh } = useAuth();
  const { choice, setChoice } = useTheme();
  const location = useLocation();
  const navigate = useNavigate();
  const badges = useNavBadges();
  const [clusterName, setClusterName] = useState<string | null>(null);
  const [networks, setNetworks] = useState<string[]>([]);
  const [moreOpen, setMoreOpen] = useState(false);
  // Absent on an older server, which has no MCP to switch off.
  const mcpEnabled = identity?.mcp_enabled !== false;
  const live = status === 'open';

  // The event stream carries raft state, not cluster.yaml, so the name and the
  // network list need their own one-shot fetch.
  useEffect(() => {
    api.getConfiguration().then(
      (cfg) => {
        setClusterName(cfg.cluster.name);
        setNetworks(Object.keys(cfg.networks ?? {}).sort());
      },
      () => {},
    );
  }, []);

  const inMore = location.pathname.startsWith('/instances') || location.pathname.startsWith('/networks');
  useEffect(() => {
    if (inMore) setMoreOpen(true);
  }, [inMore]);

  const items: SideRailEntry[] = useMemo(() => {
    const more: SideRailEntry[] = [
      { id: '/instances', label: 'Instances', icon: 'container', indent: true },
      ...networks.map((n): SideRailEntry => ({ id: `/networks/${encodeURIComponent(n)}`, label: n, icon: 'network', indent: true })),
      ...(mcpEnabled ? [{ id: '/settings/access', label: 'AI access', icon: 'sparkle', indent: true } as SideRailEntry] : []),
    ];
    return [
      { id: '/', label: 'Overview', icon: 'overview' },
      { id: '/stacks', label: 'Stacks', icon: 'stack', badge: badges.stacks },
      { id: '/nodes', label: 'Nodes', icon: 'node', badge: badges.nodes },
      { id: '/logs', label: 'Logs', icon: 'terminal' },
      { heading: 'Network & data' },
      { id: '/proxy', label: 'Proxy', icon: 'proxy' },
      { id: '/dns', label: 'DNS', icon: 'globe' },
      { id: '/storage', label: 'Storage', icon: 'archive' },
      { heading: 'System' },
      { id: '/updates', label: 'Updates', icon: 'download', badge: badges.updates },
      { id: '/settings', label: 'Settings', icon: 'settings' },
      { id: '/more-group', label: 'More', icon: 'more', expanded: moreOpen },
      ...(moreOpen ? more : []),
    ];
  }, [badges, networks, moreOpen, mcpEnabled]);

  const ids = items.flatMap((i) => ('id' in i && i.id !== '/more-group' ? [i.id] : []));
  const active = activeFor(location.pathname, ids);

  const onRail = (id: string) => {
    if (id === '/more-group') setMoreOpen((o) => !o);
    else navigate(id);
  };

  const bottom: BottomNavItem[] = [
    { id: '/', label: 'Overview', icon: 'overview' },
    { id: '/stacks', label: 'Stacks', icon: 'stack', badge: badges.stacks },
    { id: '/nodes', label: 'Nodes', icon: 'node', badge: badges.nodes },
    { id: '/logs', label: 'Logs', icon: 'terminal' },
    { id: '/more', label: 'More', icon: 'more', badge: badges.updates },
  ];
  const bottomActive = activeFor(location.pathname, bottom.map((b) => b.id)) ?? '/more';

  const logout = () => {
    api.logout().catch(() => {}).finally(() => void refresh());
  };

  return (
    <div className="shell">
      <a className="skip" href="#main">
        Skip to content
      </a>
      <aside className="shell__rail">
        <SideRail
          label="Main navigation"
          items={items}
          active={active}
          onChange={onRail}
          header={
            <span className="rail-brand" style={{ padding: 0 }}>
              <img src="/icon.svg" alt="" width={36} height={36} />
              <span className="rail-name">{clusterName ?? 'clouduler'}</span>
            </span>
          }
          footer={
            <div className="rail-foot" style={{ borderTop: 0, padding: 0 }}>
              <div className="rail-user">
                <span className="row" style={{ minWidth: 0 }}>
                  <span className={`livedot${live ? '' : ' livedot--off'}`} title={live ? 'live' : status} />
                  <span className="body-sm" title={identity?.username} style={{ overflowWrap: 'anywhere' }}>
                    {identity?.username}
                  </span>
                </span>
                {identity && <Tag tone={isAdmin ? 'pop' : undefined}>{isAdmin ? 'admin' : 'viewer'}</Tag>}
              </div>
              <div className="theme-seg" role="group" aria-label="Color theme">
                {THEMES.map((t) => (
                  <button
                    key={t.value}
                    type="button"
                    onClick={() => setChoice(t.value)}
                    aria-pressed={choice === t.value}
                    aria-label={t.label}
                    title={t.label}
                  >
                    <Icon name={t.icon} size={18} />
                  </button>
                ))}
              </div>
              <button type="button" className="cl-btn cl-btn--ghost" onClick={logout}>
                <Icon name="logout" />
                <span>Log out</span>
              </button>
            </div>
          }
        />
      </aside>

      <main id="main" className="shell__main">
        <div className="shell__col">{children}</div>
      </main>

      <div className="bnav-fixed">
        <BottomNav label="Primary" items={bottom} active={bottomActive} onChange={(id) => navigate(id)} />
      </div>
    </div>
  );
}
