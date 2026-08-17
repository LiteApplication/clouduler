import type { ReactNode } from 'react';
import { NavLink } from 'react-router-dom';
import { useClusterStore } from '../state/store';
import { useAuth } from '../auth/AuthContext';

const LINKS = [
  { to: '/', label: 'Overview', end: true },
  { to: '/nodes', label: 'Nodes' },
  { to: '/stacks', label: 'Stacks' },
  { to: '/instances', label: 'Instances' },
  { to: '/configs-secrets', label: 'Configs & Secrets' },
  { to: '/drives', label: 'Drives & Volumes' },
  { to: '/configuration', label: 'Configuration' },
  { to: '/certificates', label: 'Certificates & DNS' },
];

export function Layout({ children }: { children: ReactNode }) {
  const { status } = useClusterStore();
  const { identity, isAdmin } = useAuth();

  return (
    <div className="app">
      <nav className="sidebar" aria-label="Main navigation">
        <div className="sidebar-brand">clouduler</div>
        {LINKS.map((link) => (
          <NavLink
            key={link.to}
            to={link.to}
            end={link.end}
            className={({ isActive }) => `nav-link${isActive ? ' active' : ''}`}
          >
            {link.label}
          </NavLink>
        ))}
        <div style={{ marginTop: 'auto', paddingTop: 16, fontSize: 12 }} className="faint">
          <div>{status === 'open' ? 'live' : status}</div>
          {identity && (
            <div>
              {identity.subject} ({isAdmin ? 'admin' : 'viewer'})
            </div>
          )}
        </div>
      </nav>
      <main className="main">{children}</main>
    </div>
  );
}
