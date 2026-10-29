// `/more`: the phone's overflow screen, holding what the bottom nav has no room
// for, plus the account panel (the rail footer on desktop).

import { Link } from 'react-router-dom';
import { useAuth } from '../auth/AuthContext';
import { api } from '../api/client';
import { Button, Icon, KV, PageHeader, Panel, Tag, type IconName } from '../ds';
import { useNavBadges } from '../components/Layout';
import { useClusterState } from '../state/store';
import { useTheme, type ThemeChoice } from '../theme';

const THEMES: { value: ThemeChoice; label: string }[] = [
  { value: 'light', label: 'Light' },
  { value: 'dark', label: 'Dark' },
  { value: 'system', label: 'System' },
];

function Row({ to, icon, name, meta, tag }: { to: string; icon: IconName; name: string; meta: string; tag?: React.ReactNode }) {
  return (
    <Link className="more-row" to={to}>
      <Icon name={icon} size={22} />
      <span className="li__main grow">
        <span className="li__name">{name}</span>
        <span className="li__meta">{meta}</span>
      </span>
      {tag}
      <Icon name="chevron" size={18} />
    </Link>
  );
}

export function PhoneMore() {
  const { identity, isAdmin, refresh } = useAuth();
  const { choice, setChoice } = useTheme();
  const badges = useNavBadges();
  const cluster = useClusterState();
  const mcpEnabled = identity?.mcp_enabled !== false;
  const nodeCount = Object.keys(cluster?.nodes ?? {}).length;

  return (
    <>
      <PageHeader eyebrow="homelab" title="More" />

      <section className="col" aria-labelledby="m-net">
        <h2 className="eyebrow" id="m-net" style={{ margin: 0 }}>
          Network &amp; data
        </h2>
        <div className="list more-list">
          <Row to="/proxy" icon="proxy" name="Proxy" meta="Routes, redirects and ingress" />
          <Row to="/dns" icon="globe" name="DNS" meta="Records and certificates" />
          <Row to="/storage" icon="archive" name="Storage" meta="Volumes and backups" />
        </div>
      </section>

      <section className="col" aria-labelledby="m-sys">
        <h2 className="eyebrow" id="m-sys" style={{ margin: 0 }}>
          System
        </h2>
        <div className="list more-list">
          <Row
            to="/updates"
            icon="sync"
            name="Updates"
            meta="Images and cluster self-update"
            tag={badges.updates > 0 ? <Tag tone="danger">{badges.updates} failed</Tag> : undefined}
          />
          <Row to="/settings" icon="settings" name="Settings" meta="Cluster config, secrets, access" />
          <Row to="/instances" icon="container" name="Instances" meta="Every container in the cluster" />
          {mcpEnabled && <Row to="/settings/access" icon="sparkle" name="AI access" meta="Keys, clients and requests" />}
        </div>
      </section>

      <Panel title={identity?.username ?? 'You'} actions={identity && <Tag tone={isAdmin ? 'pop' : undefined}>{isAdmin ? 'admin' : 'viewer'}</Tag>}>
        <KV items={[['Version', identity ? (cluster ? `${nodeCount} nodes` : '-') : '-']]} />
        <div className="theme-seg" role="group" aria-label="Color theme">
          {THEMES.map((t) => (
            <button key={t.value} type="button" aria-pressed={choice === t.value} onClick={() => setChoice(t.value)}>
              {t.label}
            </button>
          ))}
        </div>
        <Button onClick={() => api.logout().catch(() => {}).finally(() => void refresh())}>Sign out</Button>
      </Panel>
    </>
  );
}
