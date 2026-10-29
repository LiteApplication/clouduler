// /settings/:tab. The config editors live in four tabs (General, Network,
// Storage, Security) that share one draft; the header's primary button follows
// the active tab.

import { useState } from 'react';
import { Navigate, useNavigate, useParams } from 'react-router-dom';
import { useAuth } from '../../auth/AuthContext';
import { Button, PageHeader, Tabs } from '../../ds';
import { ConfigTab, primaryAction, tabChanges, useSettingsConfig } from './SettingsConfig';
import { SettingsAccess } from './SettingsAccess';
import { SettingsSecrets } from './SettingsSecrets';
import { SettingsUpdate } from './SettingsUpdate';
import { SETTINGS_TABS } from './settingsTabs';
import './settings.css';

export { SETTINGS_TABS, CONFIG_SLUG_TAB } from './settingsTabs';

const CONFIG_TABS = ['general', 'network', 'storage', 'security'];

const HINTS: Record<string, string> = {
  general: 'The cluster itself and when a silent node is declared lost.',
  network: 'Networks, the web UI, the reverse proxy and Cloudflare. Risky changes roll back unless you confirm.',
  storage: 'How each drive is replicated or served across the cluster.',
  security: 'Sign-in, AI access and the encrypted git backup.',
  secrets: 'Sealed values and bundled files. Secret values are never shown.',
  access: 'Who and what can reach this cluster.',
  'self-update': 'Roll out a new Clouduler release across every node.',
};

export function Settings() {
  const { tab = 'general' } = useParams();
  const navigate = useNavigate();
  const { isAdmin } = useAuth();
  const cfg = useSettingsConfig();
  const [rollbackAfter, setRollbackAfter] = useState(60);
  const [sheet, setSheet] = useState(false);
  const [startSignal, setStartSignal] = useState(0);

  if (!SETTINGS_TABS.some((t) => t.id === tab)) return <Navigate to="/settings/general" replace />;

  const isConfig = CONFIG_TABS.includes(tab);
  const items = SETTINGS_TABS.map((t) => ({ ...t, count: CONFIG_TABS.includes(t.id) ? tabChanges(cfg.diff, t.id) : null }));

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
    } else if (tab === 'secrets') {
      primary = <Button variant="primary" icon="plus" onClick={() => setSheet(true)}>Create secret</Button>;
    } else if (tab === 'access') {
      primary = <Button variant="primary" icon="plus" onClick={() => setSheet(true)}>Create API key</Button>;
    } else {
      primary = <Button variant="primary" icon="play" onClick={() => setStartSignal((n) => n + 1)}>Start update</Button>;
    }
  }

  return (
    <div className="settings">
      <PageHeader eyebrow="System" title="Settings" sub={HINTS[tab]} actions={primary} />
      <Tabs label="Settings sections" items={items} active={tab} onChange={(id) => { setSheet(false); navigate(`/settings/${id}`); }} />
      {isConfig && <ConfigTab tab={tab} cfg={cfg} rollbackAfter={rollbackAfter} setRollbackAfter={setRollbackAfter} />}
      {tab === 'secrets' && <SettingsSecrets sheetOpen={sheet} onOpen={() => setSheet(true)} onClose={() => setSheet(false)} />}
      {tab === 'access' && <SettingsAccess sheetOpen={sheet} onOpen={() => setSheet(true)} onClose={() => setSheet(false)} />}
      {tab === 'self-update' && <SettingsUpdate startSignal={startSignal} />}
    </div>
  );
}
