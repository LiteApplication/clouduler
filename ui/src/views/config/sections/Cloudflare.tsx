import { useEffect, useState } from 'react';
import { api, ApiError } from '../../../api/client';
import type { CloudflareTokenStatus } from '../../../api/types';
import { Icon } from '../../../components/Icon';
import { CheckboxField, TextField } from '../../../components/FormFields';
import type { SectionProps } from './types';

export function CloudflareSection({ draft, setDraft, disabled }: SectionProps) {
  const cf = draft.cloudflare;
  return (
    <div className="card">
      <CheckboxField
        id="cf-enabled"
        label="Manage DNS with Cloudflare"
        value={cf != null}
        onChange={(enabled) => setDraft({ ...draft, cloudflare: enabled ? { zones: [] } : null })}
        hint="Required for automatic certificates."
        help="Lets clouduler write DNS records for network and route domains, and is required if ACME certificate issuance is turned on above (DNS-01 challenges go through Cloudflare)."
        configKey="cloudflare"
        disabled={disabled}
      />
      {cf && (
        <TextField
          id="cf-zones"
          label="DNS zones"
          value={cf.zones.join(', ')}
          onChange={(v) =>
            setDraft({
              ...draft,
              cloudflare: {
                ...cf,
                zones: v
                  .split(',')
                  .map((z) => z.trim())
                  .filter(Boolean),
              },
            })
          }
          hint="Comma separated, e.g. example.com."
          help="Zone names clouduler may write records to, e.g. example.com, other-example.com. Every network domain and the dashboard's own address must fall under one of these zones."
          configKey="cloudflare.zones"
          disabled={disabled}
        />
      )}
      {/* The token is its own sealed secret, independent of `cf` and the rest
          of the draft (see CloudflareToken). Kept settable even with DNS
          management off, so a token can be staged ahead of flipping it on. */}
      <CloudflareToken disabled={disabled} />
    </div>
  );
}

/**
 * The API token, set here rather than by naming a file the operator has to
 * create over ssh first.
 *
 * It sits outside the config draft on purpose. The token is written by its own
 * endpoint the moment Save is pressed, not on Apply, because it is not a config
 * value that can be diffed or rolled back. Sealed and replicated as a cluster
 * secret, so every node has it, and can be rotated at any time by submitting a
 * new one; it just overwrites the old.
 */
function CloudflareToken({ disabled }: { disabled: boolean }) {
  const [status, setStatus] = useState<CloudflareTokenStatus | null>(null);
  const [token, setToken] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api
      .cloudflareToken()
      .then(setStatus)
      .catch(() => setStatus(null));
  }, []);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.setCloudflareToken(token.trim());
      setToken('');
      setStatus(await api.cloudflareToken());
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'could not save the token');
    } finally {
      setBusy(false);
    }
  };

  if (!status) {
    return (
      <p className="loading">
        <span className="spinner" /> Checking the API token...
      </p>
    );
  }

  return (
    <div className="field">
      <label htmlFor="cf-token">API token</label>
      <span className="field-hint">
        Create one at the Cloudflare dashboard under My Profile &gt; API Tokens, scoped to Zone / DNS / Edit for the
        zones listed above.
      </span>
      <input
        id="cf-token"
        type="password"
        value={token}
        onChange={(e) => setToken(e.target.value)}
        placeholder="Cloudflare API token"
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
        <button onClick={save} disabled={disabled || busy || token.trim().length === 0}>
          {busy ? <span className="spinner" /> : <Icon name="key" />}
          {busy ? 'Saving...' : status.configured ? 'Rotate token' : 'Save token'}
        </button>
      </div>
    </div>
  );
}
