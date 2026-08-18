// App shell. On a wide screen this is a fixed sidebar next to the content; on
// a phone the sidebar becomes an off-canvas drawer with a top bar and a
// bottom tab bar for the destinations an operator reaches most often.

import { useEffect, useState, type ReactNode } from 'react';
import { NavLink, useLocation } from 'react-router-dom';
import { useClusterStore } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { Icon, type IconName } from './Icon';
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
}

const LINKS: NavItem[] = [
  { to: '/', label: 'Overview', short: 'Home', icon: 'overview', end: true, tab: true },
  { to: '/nodes', label: 'Nodes', short: 'Nodes', icon: 'nodes', tab: true },
  { to: '/stacks', label: 'Stacks', short: 'Stacks', icon: 'stacks', tab: true },
  { to: '/instances', label: 'Instances', short: 'Instances', icon: 'instances', tab: true },
  { to: '/configs-secrets', label: 'Configs & Secrets', short: 'Secrets', icon: 'secrets' },
  { to: '/drives', label: 'Drives & Volumes', short: 'Drives', icon: 'drives' },
  { to: '/configuration', label: 'Configuration', short: 'Config', icon: 'settings' },
  { to: '/certificates', label: 'Certificates & DNS', short: 'Certs', icon: 'certificate' },
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

        {LINKS.map((link) => (
          <NavLink
            key={link.to}
            to={link.to}
            end={link.end}
            className={({ isActive }) => `nav-link${isActive ? ' active' : ''}`}
          >
            <Icon name={link.icon} />
            {link.label}
          </NavLink>
        ))}

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
