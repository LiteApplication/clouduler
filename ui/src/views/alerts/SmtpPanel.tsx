import { useEffect, useState } from 'react';
import { api, ApiError } from '../../api/client';
import type { AlertSettingsResponse, AlertSmtp, AlertTestResult, SmtpSecurity } from '../../api/types';
import { Button, CheckOption, Panel, Tag, TextField, useConfirm, useToast } from '../../ds';
import { SelectField } from '../../components/FormFields';
import { saveBody } from '../../lib/alerts';
import { TestResult } from './TestResult';

const PORT_FOR: Record<SmtpSecurity, number> = { starttls: 587, tls: 465, none: 25 };

interface Draft {
  host: string;
  port: string;
  security: SmtpSecurity;
  username: string;
  password: string;
  clearPassword: boolean;
  from: string;
  testTo: string;
}

function draftOf(smtp: AlertSmtp | undefined): Draft {
  return {
    host: smtp?.host ?? '',
    port: String(smtp?.port ?? 587),
    security: smtp?.security ?? 'starttls',
    username: smtp?.username ?? '',
    password: '',
    clearPassword: false,
    from: smtp?.from ?? '',
    testTo: '',
  };
}

function toSmtp(d: Draft): AlertSmtp {
  return {
    host: d.host.trim(),
    port: Number(d.port),
    security: d.security,
    ...(d.username.trim() ? { username: d.username.trim() } : {}),
    from: d.from.trim(),
  };
}

export function SmtpPanel({ settings, reload }: { settings: AlertSettingsResponse; reload: () => Promise<void> }) {
  const toast = useToast();
  const confirm = useConfirm();
  const saved = settings.settings.smtp;
  const [d, setD] = useState<Draft>(() => draftOf(saved));
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [result, setResult] = useState<AlertTestResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const savedKey = JSON.stringify(saved ?? null);
  useEffect(() => {
    setD(draftOf(saved));
    setResult(null);
    // Reset only when the saved server changes, not on every health poll.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [savedKey]);

  const set = (p: Partial<Draft>) => {
    setD((x) => ({ ...x, ...p }));
    setResult(null);
  };
  const port = Number(d.port);
  const errors = {
    host: !d.host.trim() ? 'Enter the server name.' : undefined,
    port: !Number.isInteger(port) || port < 1 || port > 65535 ? 'A port between 1 and 65535.' : undefined,
    from: /^[^<>@\s]+@[^<>@\s]+$/.test(d.from.trim()) || /<[^\s@<>]+@[^\s@<>]+>$/.test(d.from.trim()) ? undefined : 'An address such as alerts@example.com or Clouduler <alerts@example.com>.',
  };
  const valid = !errors.host && !errors.port && !errors.from;
  const [shownErrors, setShown] = useState(false);
  const dirty =
    JSON.stringify(toSmtp(d)) !== JSON.stringify(saved ?? null) || d.password !== '' || d.clearPassword;

  const passwordEdit = d.clearPassword ? null : d.password ? d.password : undefined;

  const save = async () => {
    setShown(true);
    if (!valid) return;
    setSaving(true);
    setError(null);
    try {
      await api.putAlertSettings(saveBody({ ...settings.settings, smtp: toSmtp(d) }, undefined, passwordEdit));
      await reload();
      toast('Email server saved');
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Could not save.');
    } finally {
      setSaving(false);
    }
  };

  const test = async () => {
    setShown(true);
    if (!valid) return;
    if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(d.testTo.trim())) {
      setResult({ ok: false, error: 'Enter the address to send the test to.' });
      return;
    }
    setTesting(true);
    setResult(null);
    try {
      setResult(
        await api.testAlert({
          destination: {
            name: 'smtp-test',
            enabled: true,
            kind: 'email',
            to: [d.testTo.trim()],
            severities: [],
            digest: false,
          },
          secrets: {},
          smtp: toSmtp(d),
          ...(passwordEdit ? { smtp_password: passwordEdit } : {}),
        }),
      );
    } catch (e) {
      setResult({ ok: false, error: e instanceof ApiError ? e.message : 'The test could not be sent.' });
    } finally {
      setTesting(false);
    }
  };

  const remove = async () => {
    const used = settings.settings.destinations.filter((x) => x.kind === 'email').length;
    const ok = await confirm({
      title: 'Remove the email server?',
      body: used ? `${used} email destination${used === 1 ? '' : 's'} will stop sending until a server is set again.` : 'No email destination uses it.',
      confirmLabel: 'Remove',
    });
    if (!ok) return;
    try {
      const { smtp: _gone, ...rest } = settings.settings;
      void _gone;
      await api.putAlertSettings(saveBody(rest, undefined, null));
      await reload();
      toast('Email server removed');
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Could not remove it.');
    }
  };

  const e: Partial<typeof errors> = shownErrors ? errors : {};
  return (
    <Panel
      title="Email server"
      className="al-smtp"
      actions={saved ? <Tag tone="ok">Configured</Tag> : <Tag>Not set up</Tag>}
    >
      <p className="sub" id="email-server">
        One SMTP server shared by every email destination. Changes apply to the next message, no restart.
      </p>
      <div className="grid grid--3" style={{ gap: 12 }}>
        <TextField label="Host" mono placeholder="smtp.example.com" value={d.host} error={e.host} onChange={(x) => set({ host: x.target.value })} />
        <TextField label="Port" mono inputMode="numeric" value={d.port} error={e.port} onChange={(x) => set({ port: x.target.value })} />
        <SelectField<SmtpSecurity>
          id="smtp-security"
          label="Security"
          value={d.security}
          options={[
            { value: 'starttls', label: 'STARTTLS (587)' },
            { value: 'tls', label: 'TLS (465)' },
            { value: 'none', label: 'None (trusted network only)' },
          ]}
          onChange={(security) => set({ security, port: String(PORT_FOR[security]) })}
        />
        <TextField label="Username" mono value={d.username} placeholder="optional" onChange={(x) => set({ username: x.target.value })} />
        <TextField
          label="Password"
          type="password"
          mono
          value={d.password}
          disabled={d.clearPassword}
          placeholder={settings.smtp_password ? 'Saved. Blank keeps it.' : 'optional'}
          hint="Stored sealed and never shown again."
          onChange={(x) => set({ password: x.target.value })}
        />
        <TextField
          label="From"
          mono
          placeholder="Clouduler <alerts@example.com>"
          value={d.from}
          error={e.from}
          onChange={(x) => set({ from: x.target.value })}
        />
      </div>
      {settings.smtp_password && (
        <CheckOption checked={d.clearPassword} onChange={(clearPassword) => set({ clearPassword, password: '' })}>
          Remove the saved password
        </CheckOption>
      )}
      {error && <p className="al-dest__problem">{error}</p>}
      <div className="acts">
        <Button variant="primary" icon="check" loading={saving} disabled={!dirty} onClick={save}>
          Save server
        </Button>
        {saved && (
          <Button variant="ghost" icon="trash" onClick={remove}>
            Remove
          </Button>
        )}
      </div>
      <div className="al-test">
        <div style={{ flex: '1 1 220px', maxWidth: 320 }}>
          <TextField label="Send a test email to" mono inputMode="email" placeholder="you@example.com" value={d.testTo} onChange={(x) => set({ testTo: x.target.value })} />
        </div>
        <Button variant="secondary" icon="play" loading={testing} onClick={test}>
          Send test email
        </Button>
        <TestResult result={result} />
      </div>
      <p className="sub">The test uses the values above, saved or not.</p>
    </Panel>
  );
}
