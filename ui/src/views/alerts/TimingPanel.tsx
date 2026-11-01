import { useEffect, useState } from 'react';
import { api, ApiError } from '../../api/client';
import type { AlertSettings, AlertSettingsResponse } from '../../api/types';
import { Button, Panel, TextField, Toggle, useToast } from '../../ds';
import { parseDuration, saveBody, validTime } from '../../lib/alerts';

type NumKey = 'disk_warning_percent' | 'disk_critical_percent' | 'memory_percent' | 'cert_warning_days' | 'cert_critical_days';

export function TimingPanel({ settings, reload }: { settings: AlertSettingsResponse; reload: () => Promise<void> }) {
  const toast = useToast();
  const [d, setD] = useState<AlertSettings>(settings.settings);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [shown, setShown] = useState(false);
  const savedKey = JSON.stringify(settings.settings);
  useEffect(() => {
    setD(settings.settings);
    // Reset when what is saved changes, so a background refresh never eats an edit.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [savedKey]);

  const dirty = JSON.stringify(d) !== savedKey;
  const set = (p: Partial<AlertSettings>) => setD((x) => ({ ...x, ...p }));
  const num = (k: NumKey, v: string) => setD((x) => ({ ...x, thresholds: { ...x.thresholds, [k]: Number(v) || 0 } }));

  const errors = {
    debounce: parseDuration(d.warning_debounce) == null ? 'Like 5m or 1h 30m.' : undefined,
    digest: validTime(d.digest_at) ? undefined : 'A time such as 09:00.',
    retention: parseDuration(d.retention) == null ? 'Like 30days or 12h.' : undefined,
    memory_for: parseDuration(d.thresholds.memory_for) == null ? 'Like 10m.' : undefined,
    url: d.public_url && !/^https?:\/\/[^\s]+$/.test(d.public_url) ? 'Start with https://.' : undefined,
  };
  const valid = Object.values(errors).every((e) => !e);
  const e: Partial<typeof errors> = shown ? errors : {};

  const save = async () => {
    setShown(true);
    if (!valid) return;
    setSaving(true);
    setError(null);
    try {
      const clean: AlertSettings = {
        ...d,
        timezone: d.timezone?.trim() || undefined,
        public_url: d.public_url?.trim().replace(/\/+$/, '') || undefined,
      };
      await api.putAlertSettings(saveBody(clean));
      await reload();
      toast('Alert settings saved');
    } catch (err) {
      setError(err instanceof ApiError ? err.message : 'Could not save.');
    } finally {
      setSaving(false);
    }
  };

  return (
    <Panel title="Timing and limits" actions={<Button variant="primary" icon="check" loading={saving} disabled={!dirty} onClick={save}>Save</Button>}>
      <Toggle
        label="Send notifications"
        description="Off keeps the in-app feed but sends nothing to any destination."
        checked={d.enabled}
        onChange={(enabled) => set({ enabled })}
      />
      <div className="grid grid--3" style={{ gap: 12 }}>
        <TextField label="Warning debounce" mono value={d.warning_debounce} error={e.debounce} hint="A warning is sent once it has lasted this long." onChange={(x) => set({ warning_debounce: x.target.value })} />
        <TextField label="Daily digest at" mono value={d.digest_at} error={e.digest} hint="Info alerts are bundled into one message." onChange={(x) => set({ digest_at: x.target.value })} />
        <TextField label="Time zone" mono value={d.timezone ?? ''} placeholder="Europe/Paris" hint="Digest time is read in this zone. Empty uses the leader's." onChange={(x) => set({ timezone: x.target.value })} />
        <TextField label="Keep history for" mono value={d.retention} error={e.retention} hint="Resolved and one-shot alerts stay in the feed this long." onChange={(x) => set({ retention: x.target.value })} />
        <TextField label="Public URL" mono value={d.public_url ?? ''} error={e.url} placeholder="https://clouduler.example.com" hint="Base of the links in messages. Empty uses the cluster's web address." onChange={(x) => set({ public_url: x.target.value })} />
      </div>
      <div className="grid" style={{ gap: 12 }}>
        <Toggle label="Notify again when an alert gets worse" description="A warning that turns critical sends a new message." checked={d.renotify_on_escalation} onChange={(v) => set({ renotify_on_escalation: v })} />
        <Toggle label="Tell me when it is resolved" description="Sent to the destinations that heard about it opening." checked={d.notify_resolved} onChange={(v) => set({ notify_resolved: v })} />
      </div>
      <details className="al-adv">
        <summary>Thresholds</summary>
        <div className="grid grid--3" style={{ gap: 12 }}>
          <TextField label="Disk warning (% used)" mono inputMode="numeric" value={String(d.thresholds.disk_warning_percent)} onChange={(x) => num('disk_warning_percent', x.target.value)} />
          <TextField label="Disk critical (% used)" mono inputMode="numeric" value={String(d.thresholds.disk_critical_percent)} onChange={(x) => num('disk_critical_percent', x.target.value)} />
          <TextField label="Memory (% in use)" mono inputMode="numeric" value={String(d.thresholds.memory_percent)} onChange={(x) => num('memory_percent', x.target.value)} />
          <TextField label="Memory sustained for" mono value={d.thresholds.memory_for} error={e.memory_for} onChange={(x) => setD((y) => ({ ...y, thresholds: { ...y.thresholds, memory_for: x.target.value } }))} />
          <TextField label="Certificate warning (days)" mono inputMode="numeric" value={String(d.thresholds.cert_warning_days)} onChange={(x) => num('cert_warning_days', x.target.value)} />
          <TextField label="Certificate critical (days)" mono inputMode="numeric" value={String(d.thresholds.cert_critical_days)} onChange={(x) => num('cert_critical_days', x.target.value)} />
        </div>
      </details>
      {error && <p className="al-dest__problem">{error}</p>}
    </Panel>
  );
}
