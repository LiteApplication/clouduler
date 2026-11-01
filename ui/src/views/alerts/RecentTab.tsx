import { useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import type { AlertSettingsResponse, AlertSeverity } from '../../api/types';
import { Banner, Chip, EmptyState } from '../../ds';
import { SEVERITIES, SEVERITY_LABEL, groupEarlier, isNew, sentSummary, sortActive } from '../../lib/alerts';
import { useAlerts } from '../../state/alertsStore';
import { AlertRow } from './AlertRow';

type Filter = AlertSeverity | 'all';

export function RecentTab({ settings }: { settings: AlertSettingsResponse | null }) {
  const { data, error, readAt } = useAlerts();
  const [filter, setFilter] = useState<Filter>('all');

  const pass = (s: AlertSeverity) => filter === 'all' || s === filter;
  const active = useMemo(() => sortActive(data?.active ?? []).filter((a) => pass(a.severity)), [data, filter]); // eslint-disable-line react-hooks/exhaustive-deps
  const groups = useMemo(() => groupEarlier((data?.recent ?? []).filter((a) => pass(a.severity))), [data, filter]); // eslint-disable-line react-hooks/exhaustive-deps

  if (data === undefined) {
    return (
      <Banner tone="info" title="Alerts are not available">
        This node's daemon is older than the UI. Update it to see alerts.
      </Banner>
    );
  }
  if (!data) return error ? <Banner tone="danger">{error}</Banner> : <p className="muted">Loading alerts…</p>;

  const all = [...data.active, ...data.recent];
  const failedCrit = data.active.filter((a) => a.severity === 'critical' && sentSummary(a).failed.length > 0);
  const failedNames = [...new Set(failedCrit.flatMap((a) => sentSummary(a).failed.map((f) => f.destination)))];
  const dests = settings?.settings.destinations ?? [];
  const failing = dests.filter((d) => d.enabled && settings?.health[d.name]?.ok === false).length;
  const working = dests.filter((d) => d.enabled && settings?.health[d.name]?.ok !== false).length;
  const debounce = settings?.settings.warning_debounce ?? '5m';

  return (
    <>
      {error && <Banner tone="danger">{error}</Banner>}
      <div className="grid grid--3" aria-label="At a glance">
        <div className="panel al-metric">
          <span className="eyebrow">Critical now</span>
          <span className={`metric${data.counts.critical ? ' metric--crit' : ''}`} style={data.counts.critical ? { color: 'var(--danger)' } : undefined}>
            {data.counts.critical}
          </span>
          <span className="sub">
            {data.counts.critical === 0
              ? 'Nothing needs you right now.'
              : failedNames.length
                ? `Need you. ${failedCrit.length} could not be delivered to ${failedNames.join(', ')}.`
                : 'Need you.'}
          </span>
        </div>
        <div className="panel al-metric">
          <span className="eyebrow">Warnings now</span>
          <span className="metric">{data.counts.warning}</span>
          <span className="sub">Sent once they last past the {debounce} debounce.</span>
        </div>
        <div className="panel al-metric">
          <span className="eyebrow">Destinations</span>
          <span className="metric">{dests.length + 1}</span>
          <span className="sub">
            {dests.length === 0 ? (
              <>
                Only the in-app feed. <Link className="lnk" to="/alerts/destinations">Add a webhook or email.</Link>
              </>
            ) : (
              <>
                {working + 1} working{failing ? `, ${failing} failing` : ''}. <Link className="lnk" to="/alerts/destinations">See the Destinations tab.</Link>
              </>
            )}
          </span>
        </div>
      </div>

      <div className="chips" role="group" aria-label="Filter by severity">
        <Chip label="All" count={all.length} selected={filter === 'all'} onClick={() => setFilter('all')} />
        {SEVERITIES.map((s) => (
          <Chip
            key={s}
            label={SEVERITY_LABEL[s]}
            count={all.filter((a) => a.severity === s).length}
            selected={filter === s}
            onClick={() => setFilter(s)}
          />
        ))}
      </div>

      <section className="sec" aria-labelledby="act-h">
        <div className="sec__h">
          <h2 className="h2" id="act-h">Active now</h2>
          <span className="sub">Still happening. Worst first.</span>
        </div>
        {active.length === 0 ? (
          <EmptyState icon="check" title={filter === 'all' ? 'All clear' : 'Nothing active at this severity'}>
            Clouduler lists it here the moment something fires.
          </EmptyState>
        ) : (
          <div className="list">
            {active.map((a) => (
              <AlertRow key={a.id} alert={a} active isNew={isNew(a, readAt)} />
            ))}
          </div>
        )}
      </section>

      <section className="sec" aria-labelledby="old-h">
        <div className="sec__h">
          <h2 className="h2" id="old-h">Earlier</h2>
          <span className="sub">Resolved alerts and changes worth knowing about.</span>
        </div>
        {groups.length === 0 ? (
          <p className="sub">Nothing yet.</p>
        ) : (
          <div className="list">
            {groups.map((g) => (
              <div key={g.label}>
                <div className="li__group eyebrow">{g.label}</div>
                {g.items.map((a) => (
                  <AlertRow key={a.id} alert={a} isNew={isNew(a, readAt)} />
                ))}
              </div>
            ))}
          </div>
        )}
      </section>
    </>
  );
}
