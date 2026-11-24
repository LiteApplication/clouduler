import { useEffect, useState } from 'react';
import { api, ApiError } from '../../../api/client';
import type { ClusterConfig, OidcClientSecretStatus } from '../../../api/types';
import { Icon } from '../../../components/Icon';
import { CheckboxField, DurationField, TextField } from '../../../components/FormFields';
import type { SectionProps } from './types';

export function AuthSection({ draft, setDraft, disabled }: SectionProps) {
  const a = draft.auth;
  const set = (patch: Partial<ClusterConfig['auth']>) => setDraft({ ...draft, auth: { ...a, ...patch } });
  const hasOidc = a.oidc != null;

  return (
    <div className="card">
      <p className="muted">This is how people sign in to Clouduler itself. Looking for app login? See Service login.</p>
      <CheckboxField
        id="auth-apikeys"
        label="Allow API keys"
        value={a.api_keys}
        onChange={(api_keys) => set({ api_keys })}
        disabled={disabled}
        hint="Only turn off once OIDC is confirmed working."
        configKey="auth.api_keys"
      />
      <DurationField
        id="auth-ttl"
        label="Stay signed in for"
        value={a.session_ttl}
        onChange={(session_ttl) => set({ session_ttl })}
        hint="e.g. 12h."
        help="How long a browser session stays signed in before it has to log in again, e.g. 12h."
        configKey="auth.session_ttl"
        disabled={disabled}
      />
      <CheckboxField
        id="auth-oidc-enabled"
        label="Sign in with OIDC"
        value={hasOidc}
        onChange={(enabled) =>
          set({
            oidc: enabled
              ? {
                  issuer: '',
                  client_id: '',
                  scopes: ['openid', 'profile', 'email'],
                  admin_claim: 'groups',
                  admin_value: null,
                  username_claim: 'preferred_username',
                }
              : null,
          })
        }
        hint="Authentik, Keycloak, Google, Okta..."
        help="Register clouduler as an app with your identity provider (Authentik, Keycloak, Google, Okta, etc.) first, with its redirect/callback URL set to https://<the dashboard's address>/auth/oidc/callback, which is the address configured on the Web page."
        configKey="auth.oidc"
        disabled={disabled}
      />
      {a.oidc && (
        <div className="card" style={{ marginTop: 8 }}>
          <TextField
            id="oidc-issuer"
            label="Issuer URL"
            value={a.oidc.issuer}
            onChange={(issuer) => set({ oidc: { ...a.oidc!, issuer } })}
            hint="Must start with https://."
            help="Your provider's base URL. Must start with https://. Discovery is fetched from <issuer>/.well-known/openid-configuration, e.g. https://accounts.google.com or https://your-tenant.eu.auth0.com."
            configKey="auth.oidc.issuer"
            disabled={disabled}
          />
          <TextField
            id="oidc-clientid"
            label="Client ID"
            value={a.oidc.client_id}
            onChange={(client_id) => set({ oidc: { ...a.oidc!, client_id } })}
            hint="Issued by your provider."
            help="The client/application ID your provider issued when you registered clouduler as an app."
            configKey="auth.oidc.client_id"
            disabled={disabled}
          />
          {/* The client secret is its own sealed secret, independent of `a.oidc`
              and the rest of the draft (see OidcClientSecret). Kept settable
              even while OIDC is being configured, so a secret can be staged
              ahead of turning it on. */}
          <OidcClientSecret disabled={disabled} />
          <TextField
            id="oidc-usernameclaim"
            label="Username claim"
            value={a.oidc.username_claim}
            onChange={(username_claim) => set({ oidc: { ...a.oidc!, username_claim } })}
            hint="e.g. preferred_username."
            help="Name of the claim shown in the web ui as the signed-in user's name, e.g. preferred_username, email, or name. Falls back to the opaque subject id when this claim is absent."
            configKey="auth.oidc.username_claim"
            disabled={disabled}
          />
          <TextField
            id="oidc-adminclaim"
            label="Admin group claim"
            value={a.oidc.admin_claim}
            onChange={(admin_claim) => set({ oidc: { ...a.oidc!, admin_claim } })}
            hint="e.g. groups."
            help="Name of the claim in the ID token that lists the user's groups or roles, e.g. groups. Check what your provider actually sends, as it varies (roles, groups, https://your-app/roles, ...)."
            configKey="auth.oidc.admin_claim"
            disabled={disabled}
          />
          <TextField
            id="oidc-adminvalue"
            label="Admin group value"
            value={a.oidc.admin_value ?? ''}
            onChange={(v) => set({ oidc: { ...a.oidc!, admin_value: v || null } })}
            hint="Blank means nobody gets admin."
            help="A user whose admin claim contains this value gets the admin role; everyone else who signs in successfully gets read-only viewer access. Leave blank and nobody gets admin through OIDC."
            configKey="auth.oidc.admin_value"
            disabled={disabled}
          />
        </div>
      )}
    </div>
  );
}

/**
 * The client secret, set here rather than by naming a file the operator has
 * to create over ssh first.
 *
 * It sits outside the config draft on purpose, same reasoning as
 * `CloudflareToken`. The secret is written by its own endpoint the moment
 * Save is pressed, not on Apply, because it is not a config value that can be
 * diffed or rolled back. Sealed and replicated as a cluster secret, so every
 * node has it, and can be rotated at any time by submitting a new one; it
 * just overwrites the old.
 */
function OidcClientSecret({ disabled }: { disabled: boolean }) {
  return (
    <ClientSecretForm
      id="oidc-secret"
      disabled={disabled}
      load={() => api.oidcClientSecret()}
      save={(v) => api.setOidcClientSecret(v)}
      placeholder="OIDC client secret"
      hint="The client secret your provider issued alongside the client ID. Leave unset only if your provider supports a public/secretless client."
    />
  );
}

/** Write-only sealed client secret form, shared by the dashboard sign-in and
 *  the service login; only the endpoints and copy differ. */
export function ClientSecretForm({
  id,
  disabled,
  load,
  save: saveSecret,
  placeholder,
  hint,
}: {
  id: string;
  disabled: boolean;
  load: () => Promise<OidcClientSecretStatus>;
  save: (secret: string) => Promise<void>;
  placeholder: string;
  hint: string;
}) {
  const [status, setStatus] = useState<OidcClientSecretStatus | null>(null);
  const [secret, setSecret] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    load()
      .then(setStatus)
      .catch(() => setStatus(null));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await saveSecret(secret.trim());
      setSecret('');
      setStatus(await load());
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'could not save the client secret');
    } finally {
      setBusy(false);
    }
  };

  if (!status) {
    return (
      <p className="loading">
        <span className="spinner" /> Checking the client secret...
      </p>
    );
  }

  return (
    <div className="field">
      <label htmlFor={id}>Client secret</label>
      <span className="field-hint">{hint}</span>
      {status.configured && (
        <p className="muted">
          A secret is stored, replicated to every node. It cannot be shown again; enter a new one below to rotate it.
        </p>
      )}
      <input
        id={id}
        type="password"
        value={secret}
        onChange={(e) => setSecret(e.target.value)}
        placeholder={placeholder}
        autoComplete="new-password"
        disabled={disabled || busy}
      />
      {!status.configured && (
        <span className="field-hint">
          Sealed and stored as soon as you press Save, separately from the rest of this page. You will not be able to
          read it back.
        </span>
      )}
      {error && (
        <span className="field-error">
          <Icon name="alert-circle" /> {error}
        </span>
      )}
      <div className="btn-row" style={{ marginTop: 8 }}>
        <button onClick={save} disabled={disabled || busy || secret.trim().length === 0}>
          {busy ? <span className="spinner" /> : <Icon name="key" />}
          {busy ? 'Saving...' : status.configured ? 'Rotate secret' : 'Save secret'}
        </button>
      </div>
    </div>
  );
}
