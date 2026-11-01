// /alerts/:tab. Recent is the feed, Destinations is where alerts go, Events is
// every alert Clouduler can raise and how loud it is. Settings live in one
// replicated object, so the page loads them once and hands them down.

import { useCallback, useEffect, useState } from 'react';
import { Navigate, useNavigate, useParams } from 'react-router-dom';
import { api, ApiError } from '../../api/client';
import type { AlertSettingsResponse } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Banner, Button, PageHeader, Tabs } from '../../ds';
import { useAlerts } from '../../state/alertsStore';
import { DestinationsTab } from './DestinationsTab';
import { EventsTab } from './EventsTab';
import { RecentTab } from './RecentTab';
import './alerts.css';

const TABS = ['recent', 'destinations', 'events'] as const;
type Tab = (typeof TABS)[number];

export interface SettingsApi {
  res: AlertSettingsResponse | null;
  reload: () => Promise<void>;
}

function useSettings(): SettingsApi & { error: string | null } {
  const [res, setRes] = useState<AlertSettingsResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const reload = useCallback(async () => {
    try {
      setRes(await api.alertSettings());
      setError(null);
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Could not load the alert settings.');
    }
  }, []);
  useEffect(() => {
    void reload();
    // Health changes on its own (a destination starts failing), so keep it fresh.
    const id = window.setInterval(() => void reload(), 20_000);
    return () => window.clearInterval(id);
  }, [reload]);
  return { res, reload, error };
}

export function Alerts() {
  const { tab = 'recent' } = useParams();
  const navigate = useNavigate();
  const { isAdmin } = useAuth();
  const { data, unread, markAllRead } = useAlerts();
  const settings = useSettings();
  const [addOpen, setAddOpen] = useState(false);

  if (!TABS.includes(tab as Tab)) return <Navigate to="/alerts/recent" replace />;
  const t = tab as Tab;

  const active = data ? data.counts.critical + data.counts.warning + data.counts.info : 0;
  const tabs = [
    { id: 'recent', label: 'Recent', count: data?.active.length || undefined },
    { id: 'destinations', label: 'Destinations', count: settings.res ? settings.res.settings.destinations.length + 1 : undefined },
    { id: 'events', label: 'Events' },
  ];
  const sub = {
    recent:
      active > 0
        ? `${active} alert${active === 1 ? ' is' : 's are'} active. Resolved ones and good-to-know changes sit underneath.`
        : 'Nothing is wrong right now. Resolved alerts and changes worth knowing about show up here.',
    destinations: 'Where Clouduler sends alerts, and which severities go where.',
    events: 'Every alert Clouduler can raise, with its severity and where it goes.',
  }[t];

  return (
    <>
      <PageHeader
        eyebrow="Monitoring"
        title="Alerts"
        sub={sub}
        actions={
          <>
            {t === 'recent' && (
              <Button variant="secondary" icon="check" disabled={unread === 0} onClick={markAllRead}>
                Mark all as read
              </Button>
            )}
            {t === 'destinations' && isAdmin && (
              <Button variant="primary" icon="plus" onClick={() => setAddOpen(true)}>
                Add destination
              </Button>
            )}
          </>
        }
      />
      <Tabs label="Alert sections" items={tabs} active={t} onChange={(id) => navigate(`/alerts/${id}`)} />
      <div className="tabpad" role="tabpanel" aria-label={tabs.find((x) => x.id === t)?.label}>
        {settings.error && !settings.res && <Banner tone="danger">{settings.error}</Banner>}
        {t === 'recent' && <RecentTab settings={settings.res} />}
        {t === 'destinations' && (
          <DestinationsTab settings={settings.res} reload={settings.reload} addOpen={addOpen} onAddClose={() => setAddOpen(false)} />
        )}
        {t === 'events' && <EventsTab settings={settings.res} reload={settings.reload} />}
      </div>
    </>
  );
}
