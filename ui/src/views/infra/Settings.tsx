// /settings/:tab. A grouped menu on the left (a list that opens the section on
// narrow screens) and the active section beside it. The config sections share
// one draft; the header's primary button follows the active section.

import { useEffect, useState, useSyncExternalStore } from 'react';
import { Link, Navigate, useParams } from 'react-router-dom';
import { useAuth } from '../../auth/AuthContext';
import { Button, Icon, PageHeader, Tag } from '../../ds';
import { ConfigTab, primaryAction, tabChanges, useSettingsConfig } from './SettingsConfig';
import { SettingsAccess } from './SettingsAccess';
import { SettingsSecrets } from './SettingsSecrets';
import { SettingsUpdate } from './SettingsUpdate';
import { SETTINGS_GROUPS, SETTINGS_TABS } from './settingsTabs';
import './settings.css';

export { SETTINGS_TABS, CONFIG_SLUG_TAB } from './settingsTabs';

const CONFIG_TABS = ['general', 'network', 'storage', 'security'];

/** Below this width the menu and the section take turns instead of sitting
 *  side by side. */
const COMPACT = '(max-width: 1099px)';

function useCompact(): boolean {
  return useSyncExternalStore(
    (cb) => {
      const mq = window.matchMedia(COMPACT);
      mq.addEventListener('change', cb);
      return () => mq.removeEventListener('change', cb);
    },
    () => window.matchMedia(COMPACT).matches,
    () => false,
  );
}

export function Settings() {
  const { tab } = useParams();
  const compact = useCompact();
  const { isAdmin } = useAuth();
  const cfg = useSettingsConfig();
  const [rollbackAfter, setRollbackAfter] = useState(60);
  const [sheet, setSheet] = useState(false);
  const [startSignal, setStartSignal] = useState(0);

  useEffect(() => setSheet(false), [tab]);

  const current = SETTINGS_TABS.find((t) => t.id === tab);
  if (tab && !current) return <Navigate to={compact ? '/settings' : '/settings/general'} replace />;
  if (!tab && !compact) return <Navigate to="/settings/general" replace />;

  const counts = Object.fromEntries(SETTINGS_TABS.map((t) => [t.id, CONFIG_TABS.includes(t.id) ? tabChanges(cfg.diff, t.id) : 0]));
  const menu = <SettingsMenu active={tab} counts={counts} />;

  // Narrow screens: the menu is the page until a section is picked.
  if (!current) {
    return (
      <div className="settings">
        <PageHeader eyebrow="System" title="Settings" sub="Pick what you want to change." />
        {menu}
      </div>
    );
  }

  const isConfig = CONFIG_TABS.includes(current.id);
  let primary: React.ReactNode = null;
  if (isAdmin) {
    if (isConfig) {
      const p = primaryAction(cfg);
      primary = (
        <Button
          variant="primary"
          icon={p.icon}
          disabled={p.disabled}
          loading={cfg.busy}
          onClick={() => {
            if (cfg.pending?.armed) void cfg.confirm();
            else if (cfg.draft) void cfg.apply(cfg.draft, `${rollbackAfter}s`);
          }}
        >
          {p.label}
        </Button>
      );
    } else if (current.id === 'secrets') {
      primary = <Button variant="primary" icon="plus" onClick={() => setSheet(true)}>Create secret</Button>;
    } else if (current.id === 'access') {
      primary = <Button variant="primary" icon="plus" onClick={() => setSheet(true)}>Create API key</Button>;
    } else {
      primary = <Button variant="primary" icon="play" onClick={() => setStartSignal((n) => n + 1)}>Start update</Button>;
    }
  }

  return (
    <div className="settings">
      {compact && (
        <Link to="/settings" className="cl-btn cl-btn--ghost cl-btn--sm settings__back">
          <Icon name="chevron-right" size={16} className="settings__back-icon" /> Settings
        </Link>
      )}
      <PageHeader eyebrow={compact ? undefined : 'Settings'} title={current.label} sub={current.sub} actions={primary} />
      <div className="settings__body">
        {!compact && menu}
        <div className="settings__main">
          {isConfig && <ConfigTab tab={current.id} cfg={cfg} rollbackAfter={rollbackAfter} setRollbackAfter={setRollbackAfter} />}
          {current.id === 'secrets' && <SettingsSecrets sheetOpen={sheet} onClose={() => setSheet(false)} />}
          {current.id === 'access' && <SettingsAccess sheetOpen={sheet} onClose={() => setSheet(false)} />}
          {current.id === 'self-update' && <SettingsUpdate startSignal={startSignal} />}
        </div>
      </div>
    </div>
  );
}

function SettingsMenu({ active, counts }: { active?: string; counts: Record<string, number> }) {
  return (
    <nav className="snav" aria-label="Settings sections">
      {SETTINGS_GROUPS.map((group) => (
        <div className="snav__group" key={group}>
          <h2 className="snav__title">{group}</h2>
          {SETTINGS_TABS.filter((t) => t.group === group).map((t) => (
            <Link
              key={t.id}
              to={`/settings/${t.id}`}
              className="snav__item"
              aria-current={t.id === active ? 'page' : undefined}
            >
              <Icon name={t.icon} size={20} className="snav__icon" />
              <span className="snav__text">
                <span className="snav__label">{t.label}</span>
                <span className="snav__desc">{t.desc}</span>
              </span>
              {counts[t.id] > 0 && <Tag tone="brand">{counts[t.id]} unsaved</Tag>}
              <Icon name="chevron-right" size={16} className="snav__chev" />
            </Link>
          ))}
        </div>
      ))}
    </nav>
  );
}
