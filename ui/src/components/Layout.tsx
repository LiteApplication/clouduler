// App shell. On a wide screen this is a fixed sidebar next to the content; on
// a phone the sidebar becomes an off-canvas drawer with a top bar and a
// bottom tab bar for the destinations an operator reaches most often.

import { useEffect, useState, type ReactNode } from 'react';
import { NavLink, useLocation } from 'react-router-dom';
import { useClusterStore } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { Icon, type IconName } from './Icon';
import { CONFIG_GROUPS, CONFIG_PAGES } from '../views/config/pages';
import { changesFor, useConfigDraft } from '../views/config/draft';
import { Logo } from './Logo';
import { useTheme, type ThemeChoice } from '../theme';

interface NavItem {
  to: string;
  label: string;
  short: string;
  icon: IconName;
  end?: boolean;
  /** Shown in the phone tab bar as well as the drawer. */
  tab?: boolean;
  /** Expands into a submenu while the section is open. */
  submenu?: SubmenuEntry[];
}

type SubmenuEntry =
  | { heading: string }
  | { to: string; label: string; icon: IconName; keys?: string[] };

const LINKS: NavItem[] = [
  { to: '/', label: 'Overview', short: 'Home', icon: 'overview', end: true, tab: true },
  { to: '/nodes', label: 'Nodes', short: 'Nodes', icon: 'nodes', tab: true },
  { to: '/stacks', label: 'Stacks', short: 'Stacks', icon: 'stacks', tab: true },
  { to: '/instances', label: 'Instances', short: 'Instances', icon: 'instances', tab: true },
  { to: '/configs-secrets', label: 'Configs & Secrets', short: 'Secrets', icon: 'secrets' },
  { to: '/drives', label: 'Drives & Volumes', short: 'Drives', icon: 'drives' },
  {
    to: '/configuration',
    label: 'Configuration',
    short: 'Config',
    icon: 'settings',
    submenu: [
      ...CONFIG_GROUPS.flatMap((group) => [
        { heading: group } as SubmenuEntry,
        ...CONFIG_PAGES.filter((p) => p.group === group).map((p) => ({
          to: `/configuration/${p.slug}`,
          label: p.label,
          icon: p.icon,
          keys: p.keys,
        })),
      ]),
      { to: '/configuration/review', label: 'Review', icon: 'check-circle' as IconName },
    ],
  },
  { to: '/certificates', label: 'Certificates & DNS', short: 'Certs', icon: 'certificate' },
  { to: '/redirects', label: 'Redirects', short: 'Redirects', icon: 'external' },
  { to: '/self-update', label: 'Self-update', short: 'Update', icon: 'refresh' },
];

const THEMES: { value: ThemeChoice; icon: IconName; label: string }[] = [
  { value: 'light', icon: 'sun', label: 'Light theme' },
  { value: 'dark', icon: 'moon', label: 'Dark theme' },
  { value: 'system', icon: 'system', label: 'Follow system theme' },
];

export function Layout({ children }: { children: ReactNode }) {
  const { status } = useClusterStore();
  const { identity, isAdmin } = useAuth();
  const { choice, setChoice } = useTheme();
  const [drawerOpen, setDrawerOpen] = useState(false);
  const location = useLocation();

  // A drawer that survives navigation would cover the page just opened.
  useEffect(() => setDrawerOpen(false), [location.pathname]);

  useEffect(() => {
    if (!drawerOpen) return;
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && setDrawerOpen(false);
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [drawerOpen]);

  const current = LINKS.find((l) => (l.end ? location.pathname === l.to : location.pathname.startsWith(l.to)));
  const live = status === 'open';

  return (
    <div className="app">
      <header className="topbar">
        <button
          className="icon-btn ghost"
          onClick={() => setDrawerOpen(true)}
          aria-label="Open navigation"
          aria-expanded={drawerOpen}
        >
          <Icon name="menu" />
        </button>
        <span className="topbar-title">
          <Logo size={26} className="brand-mark" />
          {current?.label ?? 'clouduler'}
        </span>
        <span className="topbar-spacer" />
        <span className={`live-dot ${live ? 'on' : 'off'}`} title={live ? 'live' : status} />
      </header>

      {drawerOpen && <div className="scrim" onClick={() => setDrawerOpen(false)} aria-hidden="true" />}

      <nav className={`sidebar${drawerOpen ? ' open' : ''}`} aria-label="Main navigation">
        <NavLink to="/" className="sidebar-brand">
          <Logo size={32} className="brand-mark" />
          <span>
            clouduler
            <span className="brand-sub">cluster console</span>
          </span>
        </NavLink>

        {LINKS.map((link) => {
          const open = link.submenu && location.pathname.startsWith(link.to);
          return (
            <div key={link.to} className="nav-group">
              <NavLink
                to={link.to}
                end={link.end}
                className={({ isActive }) => `nav-link${isActive ? ' active' : ''}`}
              >
                <Icon name={link.icon} />
                {link.label}
                {link.submenu && <Icon name="chevron-right" className={`nav-caret${open ? ' open' : ''}`} />}
              </NavLink>
              {open && (
                <div className="nav-sub" role="group" aria-label={`${link.label} sections`}>
                  {link.submenu!.map((sub) =>
                    'heading' in sub ? (
                      <div key={sub.heading} className="nav-subgroup-label">
                        {sub.heading}
                      </div>
                    ) : (
                      <NavLink
                        key={sub.to}
                        to={sub.to}
                        className={({ isActive }) => `nav-link nav-sublink${isActive ? ' active' : ''}`}
                      >
                        <Icon name={sub.icon} />
                        <span className="nav-sublabel">{sub.label}</span>
                        <ConfigChanges keys={sub.keys} />
                      </NavLink>
                    ),
                  )}
                </div>
              )}
            </div>
          );
        })}

        <div className="sidebar-foot">
          <div className="status-line">
            <span className={`live-dot ${live ? 'on' : 'off'}`} />
            {live ? 'live' : status}
          </div>
          {identity && (
            <div className="identity-chip">
              <span className="avatar" aria-hidden="true">
                {identity.subject.slice(0, 2).toUpperCase()}
              </span>
              <span style={{ minWidth: 0 }}>
                <span className="identity-name" title={identity.subject}>
                  {identity.subject}
                </span>
                <span className="identity-role">
                  {isAdmin && <Icon name="crown" style={{ width: 11, height: 11, marginRight: 3 }} />}
                  {isAdmin ? 'admin' : 'viewer'}
                </span>
              </span>
            </div>
          )}
          <div className="theme-switch" role="group" aria-label="Color theme">
            {THEMES.map((t) => (
              <button
                key={t.value}
                type="button"
                onClick={() => setChoice(t.value)}
                aria-pressed={choice === t.value}
                aria-label={t.label}
                title={t.label}
              >
                <Icon name={t.icon} />
              </button>
            ))}
          </div>
        </div>
      </nav>

      <main className="main">{children}</main>

      <nav className="tabbar" aria-label="Primary">
        {LINKS.filter((l) => l.tab).map((link) => (
          <NavLink key={link.to} to={link.to} end={link.end} className={({ isActive }) => `tab${isActive ? ' active' : ''}`}>
            <Icon name={link.icon} />
            {link.short}
          </NavLink>
        ))}
        <button className="tab" onClick={() => setDrawerOpen(true)} aria-label="More">
          <Icon name="menu" />
          More
        </button>
      </nav>
    </div>
  );
}

/**
 * Unsaved-edit count for one configuration subpage, so the sidebar shows where
 * a staged change lives. With no keys (the Review entry) it counts the lot.
 */
function ConfigChanges({ keys }: { keys?: string[] }) {
  const { diff } = useConfigDraft();
  const count = keys ? changesFor(diff, keys) : diff.length;
  if (count === 0) return null;
  return (
    <span className="change-dot" aria-label={`${count} changed`}>
      {count}
    </span>
  );
}
