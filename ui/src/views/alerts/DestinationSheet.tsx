import { useMemo, useState } from 'react';
import { api, ApiError } from '../../api/client';
import type { AlertSettingsResponse, AlertSeverity, AlertTestRequest, AlertTestResult, DiscordNotify } from '../../api/types';
import { Banner, BottomSheet, Button, Chip, CheckOption, Icon, TextField, Toggle, useToast } from '../../ds';
import {
  KIND_LABEL,
  SEVERITIES,
  SEVERITY_LABEL,
  formToDestination,
  newForm,
  saveBody,
  secretEdits,
  upsertDestination,
  validateForm,
  type DestForm,
  type DestKind,
} from '../../lib/alerts';
import { DiscordPreview } from './DiscordPreview';
import { Segmented } from './Segmented';
import { TestResult } from './TestResult';

const CATEGORIES = ['cluster', 'nodes', 'workloads', 'storage', 'backups', 'edge', 'deployments', 'security'];
const NOTIFY: { value: DiscordNotify; label: string }[] = [
  { value: 'ping', label: 'Ping' },
  { value: 'normal', label: 'Normal' },
  { value: 'silent', label: 'Silent' },
];

const KIND_HINT: Record<DestKind, string> = {
  discord: 'Posts a formatted message to a Discord channel. Critical alerts are red, warnings amber, info grey.',
  webhook: 'Sends a JSON POST to any URL, for ntfy, Gotify, Home Assistant or your own service.',
  email: 'Sends a themed email through the shared email server below.',
};

const EXAMPLE_BODY = `POST  Content-Type: application/json
{
  "version": 1,
  "type": "alert",
  "cluster": "homelab",
  "event": "volume.sync_refused",
  "severity": "critical",
  "phase": "opened",
  "title": "Sync refused for media/downloads",
  "detail": "atlas-01 already holds files…",
  "node": "atlas-02",
  "link": "https://clouduler.lab.example/storage",
  "excerpt": []
}`;

/** A sealed value that is never fetched back: the masked form, and a way to replace it. */
function SealedValue({ label, shown, hint, onReplace, onRemove }: { label: string; shown: string; hint: string; onReplace: () => void; onRemove?: () => void }) {
  return (
    <div className="col" style={{ gap: 4 }}>
      <span className="cl-label">{label}</span>
      <div className="acts">
        <span className="code al-sealed">
          <Icon name="lock" size={14} /> {shown}
        </span>
        <Button size="sm" variant="secondary" onClick={onReplace}>
          Replace
        </Button>
        {onRemove && (
          <Button size="sm" variant="ghost" onClick={onRemove}>
            Remove
          </Button>
        )}
      </div>
      <span className="cl-hint">{hint}</span>
    </div>
  );
}

export function DestinationSheet({
  initial,
  settings,
  onClose,
  onSaved,
  onOpenSmtp,
}: {
  initial: DestForm;
  settings: AlertSettingsResponse;
  onClose: () => void;
  onSaved: () => Promise<void>;
  onOpenSmtp: () => void;
}) {
  const toast = useToast();
  const [f, setF] = useState<DestForm>(initial);
  const [touched, setTouched] = useState(false);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [result, setResult] = useState<AlertTestResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  const editing = f.original != null;
  const saved = editing ? settings.secrets[f.original!] : undefined;
  const savedUrl = saved?.url || null;
  const [replaceUrl, setReplaceUrl] = useState(false);
  const [replaceAuth, setReplaceAuth] = useState(false);
  const taken = settings.settings.destinations.map((d) => d.name);
  const errors = useMemo(() => validateForm(f, taken, !!savedUrl), [f, taken, savedUrl]);
  const valid = Object.keys(errors).length === 0;
  const set = (p: Partial<DestForm>) => {
    setF((x) => ({ ...x, ...p }));
    setResult(null);
  };
  const shown = <K extends keyof typeof errors>(k: K) => (touched ? errors[k] : undefined);
  const smtpMissing = f.kind === 'email' && !settings.settings.smtp;

  const switchKind = (kind: DestKind) => {
    if (editing || kind === f.kind) return;
    setF({ ...newForm(kind), name: f.name });
    setResult(null);
  };

  const toggleSev = (s: AlertSeverity) =>
    set({ severities: f.severities.includes(s) ? f.severities.filter((x) => x !== s) : [...f.severities, s] });
  const toggleCat = (c: string) =>
    set({ categories: f.categories.includes(c) ? f.categories.filter((x) => x !== c) : [...f.categories, c] });

  const test = async () => {
    setTouched(true);
    // A test needs a name only for the saved path; the others are checked by the server.
    const needs = errors.url || errors.to || errors.mention;
    if (needs) return;
    setTesting(true);
    setResult(null);
    try {
      const edits = secretEdits(f);
      const keepSaved = editing && f.kind !== 'email' && !edits?.url;
      const req: AlertTestRequest = keepSaved
        ? { name: f.original! }
        : {
            destination: formToDestination({ ...f, name: f.name.trim() || 'test' }),
            secrets: { ...(edits?.url ? { url: edits.url } : {}), ...(typeof edits?.auth === 'string' ? { auth: edits.auth } : {}) },
            ...(f.kind === 'email' && settings.settings.smtp ? { smtp: settings.settings.smtp } : {}),
          };
      setResult(await api.testAlert(req));
    } catch (e) {
      setResult({ ok: false, error: e instanceof ApiError ? e.message : 'The test could not be sent.' });
    } finally {
      setTesting(false);
    }
  };

  const save = async () => {
    setTouched(true);
    if (!valid) return;
    setSaving(true);
    setError(null);
    try {
      const dest = formToDestination(f);
      const edits = secretEdits(f);
      await api.putAlertSettings(
        saveBody(upsertDestination(settings.settings, f.original, dest), edits ? { [dest.name]: edits } : undefined),
      );
      await onSaved();
      toast(editing ? 'Destination saved' : 'Destination added');
      onClose();
    } catch (e) {
      setError(e instanceof ApiError ? (e.detail ? `${e.message}: ${e.detail}` : e.message) : 'Could not save.');
    } finally {
      setSaving(false);
    }
  };

  return (
    <BottomSheet
      open
      title={editing ? `Edit ${f.original}` : 'Add a destination'}
      onClose={onClose}
      className="al-sheet"
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="secondary" icon="play" loading={testing} onClick={test}>
            Send test message
          </Button>
          <Button variant="primary" icon={editing ? 'check' : 'plus'} loading={saving} onClick={save}>
            {editing ? 'Save' : 'Add destination'}
          </Button>
        </>
      }
    >
      <div className="col" style={{ gap: 16 }}>
        {error && <Banner tone="danger" onClose={() => setError(null)}>{error}</Banner>}

        <div className="col" style={{ gap: 8 }}>
          <span className="cl-label">Type</span>
          <div className="chips" role="group" aria-label="Destination type">
            {(['discord', 'webhook', 'email'] as DestKind[]).map((k) => (
              <Chip key={k} label={KIND_LABEL[k]} selected={f.kind === k} disabled={editing && f.kind !== k} onClick={() => switchKind(k)} />
            ))}
          </div>
          <span className="sub">{KIND_HINT[f.kind]}</span>
        </div>

        <TextField
          label="Name"
          mono
          placeholder="homelab-alerts"
          value={f.name}
          readOnly={editing}
          error={shown('name')}
          hint={editing ? 'A name cannot change. Delete and re-add to rename.' : 'Shown in this list and at the top of each message.'}
          onChange={(e) => set({ name: e.target.value })}
        />

        {f.kind !== 'email' &&
          (savedUrl && !replaceUrl ? (
            <SealedValue
              label={f.kind === 'discord' ? 'Webhook URL' : 'URL'}
              shown={savedUrl}
              hint="Stored sealed. Only the domain is ever shown."
              onReplace={() => setReplaceUrl(true)}
            />
          ) : (
            <TextField
              label={f.kind === 'discord' ? 'Webhook URL' : 'URL'}
              type="password"
              mono
              value={f.url}
              autoFocus={replaceUrl}
              placeholder={f.kind === 'discord' ? 'https://discord.com/api/webhooks/…' : 'https://ntfy.lab.example/clouduler'}
              error={shown('url')}
              hint={
                (replaceUrl ? 'Leave blank to keep the saved one. ' : '') +
                (f.kind === 'discord'
                  ? 'In Discord: channel settings, Integrations, Webhooks, Copy webhook URL. Stored sealed and never shown again.'
                  : 'Clouduler POSTs here from the leader node. Stored sealed and never shown again.')
              }
              onChange={(e) => set({ url: e.target.value })}
            />
          ))}

        {f.kind === 'discord' && (
          <>
            <div className="col" style={{ gap: 8 }}>
              <span className="cl-label">Message style</span>
              <Segmented
                label="Message style"
                value={f.style}
                options={[
                  { value: 'embed', label: 'Embed' },
                  { value: 'short', label: 'Short' },
                ]}
                onChange={(style) => set({ style })}
              />
              <DiscordPreview style={f.style} mention={f.mention} notify={f.notify} buttons={f.buttons} username={f.username} />
            </div>
            <TextField
              label="Mention on ping"
              mono
              placeholder="<@&123456789012345678>"
              value={f.mention}
              error={shown('mention')}
              hint="A role, user, @here or @everyone. In Discord, right-click the role and Copy ID, then write <@&ID>. Leave empty for no ping."
              onChange={(e) => set({ mention: e.target.value })}
            />
            <div className="col" style={{ gap: 8 }}>
              <span className="cl-label">How loud each message is</span>
              <div className="al-notify">
                {(['critical', 'warning', 'info', 'resolved'] as const).map((k) => (
                  <div key={k} style={{ display: 'contents' }}>
                    <span className="cl-label">{k === 'resolved' ? 'Resolved' : SEVERITY_LABEL[k]}</span>
                    <Segmented
                      label={`${k} notification`}
                      value={f.notify[k]}
                      options={NOTIFY}
                      onChange={(v) => set({ notify: { ...f.notify, [k]: v } })}
                    />
                  </div>
                ))}
              </div>
              <span className="sub">Ping mentions the role above. Normal is a regular message. Silent posts without a push notification.</span>
            </div>
            <Toggle
              label="Link buttons"
              description="Open in Clouduler and Logs buttons under each message. Needs a public URL."
              checked={f.buttons}
              onChange={(buttons) => set({ buttons })}
            />
            <Toggle
              label="Edit resolved messages"
              description="Update the original alert message in-place when resolved instead of sending a new message."
              checked={f.edit_resolved}
              onChange={(edit_resolved) => set({ edit_resolved })}
            />
            <TextField
              label="Sender name"
              placeholder="Clouduler"
              value={f.username}
              hint="Overrides the webhook's own name. Optional."
              onChange={(e) => set({ username: e.target.value })}
            />
          </>
        )}

        {f.kind === 'webhook' && (
          <>
            {saved?.auth_set && !replaceAuth ? (
              f.clearAuth ? (
                <div className="col" style={{ gap: 4 }}>
                  <span className="cl-label">Authorization header</span>
                  <span className="sub">It will be removed when you save.</span>
                  <div className="acts">
                    <Button size="sm" variant="ghost" onClick={() => set({ clearAuth: false })}>
                      Keep it
                    </Button>
                  </div>
                </div>
              ) : (
                <SealedValue
                  label="Authorization header"
                  shown="Configured"
                  hint="Stored sealed. Sent with every delivery."
                  onReplace={() => setReplaceAuth(true)}
                  onRemove={() => set({ clearAuth: true, auth: '' })}
                />
              )
            ) : (
              <TextField
                label="Authorization header"
                type="password"
                mono
                placeholder="Bearer …"
                value={f.auth}
                autoFocus={replaceAuth}
                hint={(replaceAuth ? 'Leave blank to keep the saved one. ' : '') + 'Stored sealed and never shown again. Sent with every delivery.'}
                onChange={(e) => set({ auth: e.target.value })}
              />
            )}
            <div className="col" style={{ gap: 8 }}>
              <span className="cl-label">Body Clouduler sends</span>
              <pre className="codeblock" tabIndex={0} aria-label="Example webhook body">
                {EXAMPLE_BODY}
              </pre>
              <span className="sub">
                Headers: <code>X-Clouduler-Event</code>, <code>X-Clouduler-Severity</code>, <code>User-Agent: clouduler/version</code>. Digests carry an <code>alerts</code> array.
              </span>
            </div>
          </>
        )}

        {f.kind === 'email' && (
          <>
            {smtpMissing && (
              <Banner tone="warn" title="No email server yet" action="Set it up" onAction={onOpenSmtp}>
                Emails go through one shared server. Fill in the Email server panel on this tab first, or this destination cannot send.
              </Banner>
            )}
            <TextField
              label="Recipients"
              multiline
              rows={3}
              mono
              placeholder="ops@example.com"
              value={f.to}
              error={shown('to')}
              hint="One per line, or separated by commas."
              onChange={(e) => set({ to: e.target.value })}
            />
          </>
        )}

        <div className="col" style={{ gap: 8 }}>
          <span className="cl-label">Send these severities</span>
          <div className="chips" role="group" aria-label="Severities">
            {SEVERITIES.filter((s) => s !== 'info').map((s) => (
              <Chip key={s} label={SEVERITY_LABEL[s]} selected={f.severities.includes(s)} onClick={() => toggleSev(s)} />
            ))}
            <Chip label="Info, as a daily digest" selected={f.digest} onClick={() => set({ digest: !f.digest })} />
          </div>
          {shown('severities') && <span className="cl-hint cl-hint--error">{errors.severities}</span>}
          <span className="sub">
            Warnings are sent once they last past the debounce. The digest is one message a day with every info alert.
          </span>
        </div>

        <details className="al-adv">
          <summary>Only some alerts</summary>
          <div className="col" style={{ gap: 12 }}>
            <div className="col" style={{ gap: 8 }}>
              <span className="cl-label">Only these areas</span>
              <div className="chips" role="group" aria-label="Areas">
                {CATEGORIES.map((c) => (
                  <Chip key={c} label={c[0].toUpperCase() + c.slice(1)} selected={f.categories.includes(c)} onClick={() => toggleCat(c)} />
                ))}
              </div>
              <span className="sub">None selected means every area.</span>
            </div>
            <TextField
              label="Only these stacks"
              mono
              placeholder="media, docs"
              value={f.stacks}
              hint="Cluster-wide alerts (nodes, quorum) always pass. Empty means every stack."
              onChange={(e) => set({ stacks: e.target.value })}
            />
            <CheckOption checked={f.severities.includes('info')} onChange={() => toggleSev('info')}>
              Also send each info alert immediately
            </CheckOption>
          </div>
        </details>

        {editing && (
          <Toggle label="Enabled" description="A disabled destination keeps its settings but receives nothing." checked={f.enabled} onChange={(enabled) => set({ enabled })} />
        )}

        <div className="al-test">
          <TestResult result={result} />
        </div>
      </div>
    </BottomSheet>
  );
}
