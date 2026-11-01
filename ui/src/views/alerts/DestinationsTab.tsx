import { useState } from 'react';
import { api, ApiError } from '../../api/client';
import type { AlertDestination, AlertSettingsResponse, AlertTestResult, DestinationHealth } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Banner, Button, EmptyState, Icon, KV, StatusBadge, Tag, useConfirm, useToast } from '../../ds';
import { IN_APP, KIND_LABEL, SEVERITIES, SEVERITY_LABEL, destinationReceives, formFrom, newForm, removeDestination, routingSummary, saveBody } from '../../lib/alerts';
import { relativeTime } from '../../lib/format';
import { DestinationSheet } from './DestinationSheet';
import { SmtpPanel } from './SmtpPanel';
import { TestResult } from './TestResult';
import { TimingPanel } from './TimingPanel';
import type { DestForm } from '../../lib/alerts';

function statusOf(d: AlertDestination, h: DestinationHealth | undefined) {
  if (!d.enabled) return { status: 'stopped' as const, label: 'Disabled' };
  if (h?.ok === false) return { status: 'failed' as const, label: 'Failing' };
  if (h?.ok === true) return { status: 'healthy' as const, label: 'Working' };
  return { status: 'stopped' as const, label: 'Not used yet' };
}

function lastSend(h: DestinationHealth | undefined): string {
  if (!h || h.ok == null || !h.at) return 'Nothing sent yet';
  return `${relativeTime(h.at)}, ${h.ok ? 'delivered' : 'failed'}`;
}

export function DestinationsTab({
  settings,
  reload,
  addOpen,
  onAddClose,
}: {
  settings: AlertSettingsResponse | null;
  reload: () => Promise<void>;
  addOpen: boolean;
  onAddClose: () => void;
}) {
  const { isAdmin } = useAuth();
  const toast = useToast();
  const confirm = useConfirm();
  const [editing, setEditing] = useState<DestForm | null>(null);
  const [tests, setTests] = useState<Record<string, AlertTestResult | 'busy'>>({});

  if (!settings) return <p className="muted">Loading destinations…</p>;
  const { settings: s, health, secrets } = settings;

  const test = async (name: string) => {
    setTests((t) => ({ ...t, [name]: 'busy' }));
    let r: AlertTestResult;
    try {
      r = await api.testAlert({ name });
    } catch (e) {
      r = { ok: false, error: e instanceof ApiError ? e.message : 'The test could not be sent.' };
    }
    setTests((t) => ({ ...t, [name]: r }));
    void reload();
  };

  const remove = async (d: AlertDestination) => {
    const ok = await confirm({
      title: `Delete ${d.name}?`,
      body: 'Its saved URL and secrets are deleted too. Events routed to it fall back to their default routing.',
      confirmLabel: 'Delete',
    });
    if (!ok) return;
    try {
      await api.putAlertSettings(saveBody(removeDestination(s, d.name)));
      await reload();
      toast(`Deleted ${d.name}.`);
    } catch (e) {
      toast(e instanceof ApiError ? e.message : 'Could not delete the destination.', { tone: 'danger' });
    }
  };

  const openSmtp = () => {
    setEditing(null);
    onAddClose();
    window.setTimeout(() => document.getElementById('email-server')?.scrollIntoView({ behavior: 'smooth', block: 'center' }), 50);
  };

  const failing = s.destinations.filter((d) => d.enabled && health[d.name]?.ok === false);
  const routing = routingSummary(s);
  const sheet = editing ?? (addOpen ? newForm('discord') : null);
  const kindLabel = (d: AlertDestination) => KIND_LABEL[d.kind];

  return (
    <>
      {failing.map((d) => {
        const h = health[d.name];
        return (
          <Banner
            key={d.name}
            tone="danger"
            title={`${d.name} has failed ${h.consecutive_failures || 'its last'} deliver${h.consecutive_failures === 1 ? 'y' : 'ies'}${h.consecutive_failures > 1 ? ' in a row' : ''}`}
            action={isAdmin ? `Send test to ${d.name}` : undefined}
            onAction={() => void test(d.name)}
          >
            {h.error ? `${h.error}. ` : ''}Critical alerts still reach the in-app feed and the other destinations.
          </Banner>
        );
      })}

      <section className="sec" aria-labelledby="dest-h">
        <div className="sec__h">
          <h2 className="h2" id="dest-h">Destinations</h2>
          <span className="sub">Add as many as you like. In-app is always on.</span>
        </div>
        <div className="grid">
          <div className="panel al-dest">
            <div className="panel__h">
              <h3 className="h3">{IN_APP}</h3>
              <Tag>Built in</Tag>
            </div>
            <StatusBadge status="healthy" label="Always on" />
            <KV items={[['Receives', 'Critical, warning and info'], ['Kept for', s.retention]]} />
          </div>
          {s.destinations.map((d) => {
            const h = health[d.name];
            const st = statusOf(d, h);
            const t = tests[d.name];
            const maskedUrl = secrets[d.name]?.url;
            return (
              <div key={d.name} className={`panel al-dest${d.enabled ? '' : ' al-dest--off'}`}>
                <div className="panel__h">
                  <h3 className="h3">{d.name}</h3>
                  <Tag tone={d.kind === 'discord' ? 'brand' : d.kind === 'email' ? 'ok' : 'pop'}>{kindLabel(d)}</Tag>
                </div>
                <StatusBadge status={st.status} label={st.label} />
                <KV
                  items={[
                    ...(d.kind === 'email'
                      ? ([['To', d.to.join(', ')]] as [string, string][])
                      : ([['Address', maskedUrl || 'Missing']] as [string, string][])),
                    ['Receives', destinationReceives(d)],
                    ['Last send', lastSend(h)],
                  ]}
                />
                {d.kind === 'email' && !s.smtp && <p className="al-dest__problem">No email server is set up yet. See the Email server panel below.</p>}
                {h?.ok === false && h.error && <p className="al-dest__problem">{h.error}</p>}
                {isAdmin && (
                  <div className="acts">
                    <Button variant="secondary" icon="play" label={`Send test to ${d.name}`} loading={t === 'busy'} onClick={() => void test(d.name)}>
                      Send test
                    </Button>
                    <Button variant="ghost" icon="edit" label={`Edit ${d.name}`} onClick={() => setEditing(formFrom(d))}>
                      Edit
                    </Button>
                    <Button variant="ghost" icon="trash" iconOnly label={`Delete ${d.name}`} onClick={() => void remove(d)} />
                  </div>
                )}
                {t && t !== 'busy' && <TestResult result={t} />}
              </div>
            );
          })}
        </div>
        {s.destinations.length === 0 && (
          <EmptyState
            icon="alert"
            title="Only the in-app feed so far"
            action={isAdmin ? <Button variant="primary" icon="plus" onClick={() => setEditing(newForm('discord'))}>Add a destination</Button> : undefined}
          >
            Add a Discord channel, a webhook for ntfy or Home Assistant, or an email address to be told when something breaks.
          </EmptyState>
        )}
      </section>

      {isAdmin && <SmtpPanel settings={settings} reload={reload} />}

      <section className="sec" aria-labelledby="rules-h">
        <div className="sec__h">
          <h2 className="h2" id="rules-h">What goes where</h2>
          <span className="sub">Severity decides who hears about it. Override single events on the Events tab.</span>
        </div>
        <div className="list">
          {SEVERITIES.map((sev) => (
            <div className="al" key={sev}>
              <div className="al__sev">
                {sev === 'critical' ? <StatusBadge status="failed" label="Critical" /> : sev === 'warning' ? <StatusBadge status="degraded" label="Warning" /> : <Tag><Icon name="info" size={14} />Info</Tag>}
              </div>
              <div className="al__main">
                <span className="al__t">
                  {sev === 'critical' ? 'Notify immediately' : sev === 'warning' ? `Notify after the problem lasts ${s.warning_debounce}` : 'Never wakes anyone'}
                </span>
                <span className="sub">
                  {sev === 'critical'
                    ? 'Data at risk, no quorum, or a service down with no automatic recovery.'
                    : sev === 'warning'
                      ? 'Something is failing but Clouduler is retrying or has a fallback.'
                      : `Goes to the feed, and to one digest message a day at ${s.digest_at}.`}
                </span>
              </div>
              <div className="al__side">
                <span className="sub" aria-label={`${SEVERITY_LABEL[sev]} goes to`}>{routing[sev].join(', ')}</span>
              </div>
            </div>
          ))}
        </div>
      </section>

      {isAdmin && <TimingPanel settings={settings} reload={reload} />}

      {sheet && (
        <DestinationSheet
          key={sheet.original ?? 'new'}
          initial={sheet}
          settings={settings}
          onClose={() => {
            setEditing(null);
            onAddClose();
          }}
          onSaved={reload}
          onOpenSmtp={openSmtp}
        />
      )}
    </>
  );
}
