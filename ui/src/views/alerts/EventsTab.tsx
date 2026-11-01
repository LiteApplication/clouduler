import { useCallback, useEffect, useMemo, useState } from 'react';
import { api, ApiError } from '../../api/client';
import type { AlertCatalogEntry, AlertSettingsResponse, AlertSeverity } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Banner, BottomSheet, Button, CheckOption, Chip, Icon, StatusBadge, Tag, useToast } from '../../ds';
import {
  IN_APP,
  SEVERITIES,
  SEVERITY_LABEL,
  buildOverride,
  filterCatalog,
  isCustom,
  routeDestinations,
  routeKind,
  routeLabel,
  saveBody,
  withOverride,
  type RouteKind,
} from '../../lib/alerts';
import { Segmented } from './Segmented';

const cap = (s: string) => s[0].toUpperCase() + s.slice(1);

function SeverityCell({ sev }: { sev: AlertSeverity }) {
  if (sev === 'critical') return <StatusBadge status="failed" label="Critical" />;
  if (sev === 'warning') return <StatusBadge status="degraded" label="Warning" />;
  return (
    <Tag>
      <Icon name="info" size={14} />
      Info
    </Tag>
  );
}

export function EventsTab({ settings, reload }: { settings: AlertSettingsResponse | null; reload: () => Promise<void> }) {
  const { isAdmin } = useAuth();
  const [catalog, setCatalog] = useState<AlertCatalogEntry[] | null | undefined>(null);
  const [error, setError] = useState<string | null>(null);
  const [cat, setCat] = useState('all');
  const [sev, setSev] = useState<AlertSeverity | 'all'>('all');
  const [edit, setEdit] = useState<AlertCatalogEntry | null>(null);

  const load = useCallback(async () => {
    try {
      setCatalog(await api.alertCatalog());
      setError(null);
    } catch (e) {
      if (e instanceof ApiError && e.status === 404) setCatalog(undefined);
      else setError(e instanceof ApiError ? e.message : 'Could not load the events.');
    }
  }, []);
  useEffect(() => void load(), [load]);

  const categories = useMemo(() => [...new Set((catalog ?? []).map((e) => e.category))], [catalog]);
  const rows = useMemo(() => filterCatalog(catalog ?? [], cat, sev), [catalog, cat, sev]);

  if (catalog === undefined) {
    return <Banner tone="info" title="Events are not available">This node's daemon is older than the UI.</Banner>;
  }
  if (!catalog) return error ? <Banner tone="danger">{error}</Banner> : <p className="muted">Loading events…</p>;

  return (
    <>
      {error && <Banner tone="danger">{error}</Banner>}
      <div className="col">
        <div className="chips" role="group" aria-label="Filter by area">
          <Chip label="All" count={catalog.length} selected={cat === 'all'} onClick={() => setCat('all')} />
          {categories.map((c) => (
            <Chip key={c} label={cap(c)} count={catalog.filter((e) => e.category === c).length} selected={cat === c} onClick={() => setCat(c)} />
          ))}
        </div>
        <div className="chips" role="group" aria-label="Filter by severity">
          <Chip label="All" selected={sev === 'all'} onClick={() => setSev('all')} />
          {SEVERITIES.map((s) => (
            <Chip key={s} label={SEVERITY_LABEL[s]} selected={sev === s} onClick={() => setSev(s)} />
          ))}
        </div>
        <p className="sub">
          A row covers an event and its recovery where there is one. When an alert escalates, the new severity is sent as a new notification.
        </p>
      </div>

      <div className="cl-tablewrap">
        <table className="cl-table">
          <caption className="cl-sr">Alert events and where they are sent</caption>
          <thead>
            <tr>
              <th scope="col" className="cl-label">Event</th>
              <th scope="col" className="cl-label">Severity</th>
              <th scope="col" className="cl-label">Goes to</th>
              <th scope="col" className="cl-label cl-right"><span className="cl-sr">Actions</span></th>
            </tr>
          </thead>
          <tbody>
            {rows.map((e) => (
              <tr key={e.key}>
                <td data-label="Event">
                  <span className="li__name">{e.title}</span>
                  <br />
                  <span className="code-sm muted">{e.key}</span>
                </td>
                <td data-label="Severity">
                  <SeverityCell sev={e.effective_severity} />
                  {e.override?.severity && e.override.severity !== e.severity && <> <span className="sub">default {e.severity}</span></>}
                  {e.note && (
                    <>
                      <br />
                      <span className="sub">{e.note}</span>
                    </>
                  )}
                </td>
                <td data-label="Goes to">
                  <span className="sub">{routeLabel(e)}</span> {isCustom(e) && <Tag tone="brand">Custom</Tag>}
                </td>
                <td data-label="Actions" className="cl-right">
                  {isAdmin && <Button variant="ghost" icon="chevron" iconOnly label={`Edit routing for ${e.key}`} onClick={() => setEdit(e)} />}
                </td>
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={4} className="sub">No events match these filters.</td>
              </tr>
            )}
          </tbody>
        </table>
      </div>

      {edit && settings && (
        <EventSheet
          entry={edit}
          settings={settings}
          onClose={() => setEdit(null)}
          onSaved={async () => {
            await Promise.all([reload(), load()]);
            setEdit(null);
          }}
        />
      )}
    </>
  );
}

function EventSheet({
  entry,
  settings,
  onClose,
  onSaved,
}: {
  entry: AlertCatalogEntry;
  settings: AlertSettingsResponse;
  onClose: () => void;
  onSaved: () => Promise<void>;
}) {
  const toast = useToast();
  const current = settings.settings.events[entry.key] ?? entry.override;
  const [sev, setSev] = useState<AlertSeverity | 'default'>(current?.severity ?? 'default');
  const [kind, setKind] = useState<RouteKind>(routeKind(current));
  const [dests, setDests] = useState<string[]>(routeDestinations(current));
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const names = settings.settings.destinations.map((d) => d.name);

  const save = async (reset?: boolean) => {
    setSaving(true);
    setError(null);
    try {
      const o = reset ? undefined : buildOverride(sev, kind, dests);
      await api.putAlertSettings(saveBody(withOverride(settings.settings, entry.key, o)));
      toast(reset ? 'Back to default routing' : 'Routing saved');
      await onSaved();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Could not save.');
      setSaving(false);
    }
  };

  const toggle = (n: string) => setDests((d) => (d.includes(n) ? d.filter((x) => x !== n) : [...d, n]));
  const none = kind === 'destinations' && dests.length === 0;

  return (
    <BottomSheet
      open
      title={entry.title}
      onClose={onClose}
      footer={
        <>
          {(current?.severity || current?.route) && (
            <Button variant="ghost" onClick={() => void save(true)} disabled={saving}>
              Reset to default
            </Button>
          )}
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" icon="check" loading={saving} disabled={none} onClick={() => void save()}>
            Save
          </Button>
        </>
      }
    >
      <div className="col" style={{ gap: 16 }}>
        {error && <Banner tone="danger">{error}</Banner>}
        <p className="sub">
          <span className="code-sm">{entry.key}</span>
          {entry.note ? ` · ${entry.note}` : ''}
        </p>
        <div className="col" style={{ gap: 8 }}>
          <span className="cl-label">Severity</span>
          <Segmented
            label="Severity"
            value={sev}
            options={[{ value: 'default', label: `Default (${entry.severity})` }, ...SEVERITIES.map((s) => ({ value: s, label: SEVERITY_LABEL[s] }))]}
            onChange={setSev}
          />
          <span className="sub">Changes how loud this event is and which destinations take it by severity.</span>
        </div>
        <div className="col" style={{ gap: 8 }}>
          <span className="cl-label">Goes to</span>
          <Segmented<RouteKind>
            label="Route"
            value={kind}
            options={[
              { value: 'default', label: 'Default' },
              { value: 'in_app', label: 'In-app only' },
              { value: 'destinations', label: 'Pick destinations' },
              { value: 'muted', label: 'Muted' },
            ]}
            onChange={setKind}
          />
          <span className="sub">
            {kind === 'default' && `Follows its severity: ${entry.routes.join(', ') || IN_APP}.`}
            {kind === 'in_app' && 'Recorded in the feed, never sent anywhere.'}
            {kind === 'muted' && 'Not recorded and not sent. You will not see it at all.'}
            {kind === 'destinations' && 'The feed plus exactly these destinations, whatever their severity filter says.'}
          </span>
          {kind === 'destinations' &&
            (names.length === 0 ? (
              <p className="sub">There are no destinations yet. Add one on the Destinations tab.</p>
            ) : (
              <div className="col" style={{ gap: 4 }}>
                {names.map((n) => (
                  <CheckOption key={n} checked={dests.includes(n)} onChange={() => toggle(n)}>
                    {n}
                  </CheckOption>
                ))}
              </div>
            ))}
        </div>
      </div>
    </BottomSheet>
  );
}
