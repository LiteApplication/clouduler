import { api } from '../../../api/client';
import type { PortalPalette, ProxyAuthConfig } from '../../../api/types';
import { CheckOption } from '../../../ds';
import { Icon } from '../../../components/Icon';
import { RememberedIps } from '../RememberedIps';
import { CheckboxField, DurationField, SelectField, TextField } from '../../../components/FormFields';
import { addressedNodesOn, isManaged, managedPortalError } from '../../../lib/managedNetwork';
import type { SectionProps } from './types';
import { ClientSecretForm } from './Auth';

const PORTAL_PALETTES: Record<PortalPalette, { label: string; colors: [string, string, string]; bg: string }> = {
  grape: {
    label: 'Grape',
    colors: ['#ff5cae', '#c8f542', '#2fd0a8'],
    bg: '#150f1f',
  },
  sunset: {
    label: 'Sunset',
    colors: ['#ff8a3d', '#5cc8ff', '#ffd23d'],
    bg: '#1c1008',
  },
  ocean: {
    label: 'Ocean',
    colors: ['#2fd0e8', '#ff7a5c', '#ffd23d'],
    bg: '#08161f',
  },
  forest: {
    label: 'Forest',
    colors: ['#ffd23d', '#ff5cae', '#7be0a0'],
    bg: '#0c1a12',
  },
};

const DEFAULT_SERVICE_LOGIN: ProxyAuthConfig = {
  host: 'login',
  networks: [],
  issuer: '',
  client_id: '',
  scopes: ['openid', 'profile', 'email', 'groups'],
  groups_claim: 'groups',
  username_claim: 'preferred_username',
  session_ttl: '12h',
  remember_ip: [],
  remember_ip_ttl: '30d',
  title: 'Sign in to continue',
  palette: 'grape',
  accent: null,
};

/**
 * `proxy.auth`: the login shown to visitors of apps whose routes use the
 * `authenticated` middleware. Deliberately separate from the dashboard
 * sign-in (`auth.oidc`): its own provider client, secret and groups.
 */
export function ServiceLoginSection({ draft, setDraft, disabled }: SectionProps) {
  const a = draft.proxy.auth ?? null;
  const setAuth = (next: ProxyAuthConfig | null) => setDraft({ ...draft, proxy: { ...draft.proxy, auth: next } });
  const patch = (p: Partial<ProxyAuthConfig>) => a && setAuth({ ...a, ...p });
  const nets = Object.entries(draft.networks);
  const palette = a ? (PORTAL_PALETTES[a.palette] ?? PORTAL_PALETTES.grape) : PORTAL_PALETTES.grape;
  const validAccent = !!a?.accent && /^#[0-9a-fA-F]{6}$/.test(a.accent);

  return (
    <div className="card">
      <div className="banner banner-info">
        <Icon name="info" />{' '}
        <span>
          This logs visitors of your <strong>apps</strong> in: the apps whose proxy routes use the{' '}
          <code>authenticated</code> middleware. It is not how you sign in to Clouduler (see Sign-in), and it uses its
          own identity provider client and groups.
        </span>
      </div>
      <CheckboxField
        id="svclogin-enabled"
        label="Enable service login"
        value={a != null}
        onChange={(enabled) => setAuth(enabled ? { ...DEFAULT_SERVICE_LOGIN } : null)}
        hint="Off means routes using the authenticated middleware are rejected."
        help="Registers a login portal on each listed network. Apps with the authenticated route middleware send visitors there, and only let in people in an allowed group."
        configKey="proxy.auth"
        disabled={disabled}
      />
      {a && (
        <div className="card" style={{ marginTop: 8 }}>
          {(a.remember_ip ?? []).length > 0 && <RememberedIps />}
          {(a.remember_ip ?? []).length > 0 && (
            <DurationField
              id="svclogin-remember-ttl"
              label="Remember an IP address for"
              value={a.remember_ip_ttl ?? '30d'}
              onChange={(remember_ip_ttl) => patch({ remember_ip_ttl })}
              hint="e.g. 30d. Counted from the login that saved it, separate from the session length."
              help="On the networks ticked above, a visitor can save their IP address after logging in; that address then skips the login until this long has passed. Only offered to visitors whose address is inside the network, and anyone behind a shared address (a router doing NAT) is let in too."
              configKey="proxy.auth.remember_ip_ttl"
              disabled={disabled}
            />
          )}
          <TextField
            id="svclogin-host"
            label="Portal host"
            value={a.host}
            onChange={(host) => patch({ host })}
            hint="A label such as login, qualified with each listed network's domain."
            help="The hostname label of the login portal. It is combined with the DNS domain of every network listed below."
            configKey="proxy.auth.host"
            disabled={disabled}
          />
          {a.networks.some((n) => draft.networks[n]?.type === 'public') && (
            <CheckboxField
              id="svclogin-cloudflare-proxy"
              label="Proxy the portal through Cloudflare"
              value={!!a.cloudflare_proxy}
              onChange={(cloudflare_proxy) => patch({ cloudflare_proxy })}
              hint="Publishes the portal's DNS records with Cloudflare's proxy (orange cloud) on, on the listed public networks."
              help="Only public networks are affected. The portal host is a single name, so the whole portal is either proxied or not; another route or redirect on the same hostname that leaves its own Cloudflare proxy off keeps the record DNS-only."
              configKey="proxy.auth.cloudflare_proxy"
              disabled={disabled}
            />
          )}
          <div className="field">
            <label>Networks</label>
            <span className="field-hint">
              Networks that offer the login. Visitors log in once per network. An app can only require login on a listed
              network.
            </span>
            {nets.length === 0 && <p className="muted">No networks are defined yet.</p>}
            {nets.map(([name, net]) => {
              const hasDomain = !!net.domain;
              const checked = a.networks.includes(name);
              const portalErr = managedPortalError(draft, name, 'the login page');
              const managedNote = isManaged(net) && !portalErr
                ? `served from the nodes that list it (${addressedNodesOn(draft, name).join(', ')}); only the client DNS server resolves it, never public DNS`
                : null;
              return (
                <div key={name}>
                  <CheckOption
                    checked={checked}
                    disabled={disabled || (!hasDomain && !checked) || (!!portalErr && !checked)}
                    hint={(!hasDomain && '(no DNS domain, set one on the Networks page)') || (portalErr && '(managed network: no node lists it)') || (managedNote && `(${managedNote})`)}
                    onChange={(on) =>
                      patch({
                        networks: on ? [...a.networks, name] : a.networks.filter((n) => n !== name),
                        remember_ip: on ? a.remember_ip : (a.remember_ip ?? []).filter((n) => n !== name),
                      })
                    }
                  >
                    {name}
                  </CheckOption>
                  {checked && ['internal', 'private', 'public'].includes(net.type) && (
                    <CheckOption
                      className="cl-check--nested"
                      checked={(a.remember_ip ?? []).includes(name)}
                      disabled={disabled}
                      onChange={(on) =>
                        patch({
                          remember_ip: on
                            ? [...(a.remember_ip ?? []), name]
                            : (a.remember_ip ?? []).filter((n) => n !== name),
                        })
                      }
                    >
                      Let visitors remember their IP address
                    </CheckOption>
                  )}
                </div>
              );
            })}
            {a.networks.length === 0 && (
              <span className="field-error">
                <Icon name="alert-circle" /> Pick at least one network.
              </span>
            )}
          </div>
          {a.networks.length > 0 && a.host && (
            <div className="field">
              <span className="field-hint">
                Portal hostnames. Your identity provider's client needs the redirect URI{' '}
                <code>https://&lt;portal host&gt;/callback</code> for each:
              </span>
              <ul data-testid="svclogin-portals">
                {a.networks.map((n) => (
                  <li key={n}>
                    <code>
                      {a.host}.{draft.networks[n]?.domain ?? '?'}
                    </code>
                    {' → '}
                    <code>
                      https://{a.host}.{draft.networks[n]?.domain ?? '?'}
                      /callback
                    </code>
                  </li>
                ))}
              </ul>
            </div>
          )}
          <TextField
            id="svclogin-issuer"
            label="Issuer URL"
            value={a.issuer}
            onChange={(issuer) => patch({ issuer })}
            hint="Must start with https://."
            help="The base URL of the identity provider used to log your apps' visitors in. Must be https. Discovery is fetched from <issuer>/.well-known/openid-configuration."
            configKey="proxy.auth.issuer"
            disabled={disabled}
          />
          <TextField
            id="svclogin-clientid"
            label="Client ID"
            value={a.client_id}
            onChange={(client_id) => patch({ client_id })}
            hint="Required. Not the same client as the dashboard sign-in."
            help="The client/application ID registered for the service login, separate from the one used for the dashboard."
            configKey="proxy.auth.client_id"
            disabled={disabled}
          />
          <ClientSecretForm
            id="svclogin-secret"
            disabled={disabled}
            load={() => api.proxyAuthClientSecret()}
            save={(v) => api.setProxyAuthClientSecret(v)}
            placeholder="Service login client secret"
            hint="The client secret of the service login client. Stored separately from the dashboard sign-in secret."
          />
          <TextField
            id="svclogin-scopes"
            label="Scopes"
            value={a.scopes.join(' ')}
            onChange={(v) => patch({ scopes: v.split(/[\s,]+/).filter(Boolean) })}
            hint="Space separated. Must include openid."
            help="OIDC scopes requested at login. openid is required; groups is usually needed for the groups claim."
            configKey="proxy.auth.scopes"
            disabled={disabled}
          />
          {!a.scopes.includes('openid') && (
            <span className="field-error">
              <Icon name="alert-circle" /> Scopes must include openid.
            </span>
          )}
          <TextField
            id="svclogin-groupsclaim"
            label="Groups claim"
            value={a.groups_claim}
            onChange={(groups_claim) => patch({ groups_claim })}
            hint="e.g. groups."
            help="Name of the token claim listing the visitor's groups, used by a route's group restriction."
            configKey="proxy.auth.groups_claim"
            disabled={disabled}
          />
          <TextField
            id="svclogin-usernameclaim"
            label="Username claim"
            value={a.username_claim}
            onChange={(username_claim) => patch({ username_claim })}
            hint="e.g. preferred_username."
            help="Name of the claim holding the visitor's username, passed to apps."
            configKey="proxy.auth.username_claim"
            disabled={disabled}
          />
          <DurationField
            id="svclogin-ttl"
            label="Stay signed in for"
            value={a.session_ttl}
            onChange={(session_ttl) => patch({ session_ttl })}
            hint="e.g. 12h."
            help="How long a visitor stays logged in to your apps before logging in again."
            configKey="proxy.auth.session_ttl"
            disabled={disabled}
          />
          <TextField
            id="svclogin-title"
            label="Portal title"
            value={a.title}
            onChange={(title) => patch({ title })}
            hint="Headline on the login page."
            configKey="proxy.auth.title"
            disabled={disabled}
          />
          <SelectField<PortalPalette>
            id="svclogin-palette"
            label="Palette"
            value={a.palette}
            onChange={(p) => patch({ palette: p })}
            options={(Object.keys(PORTAL_PALETTES) as PortalPalette[]).map((k) => ({
              value: k,
              label: PORTAL_PALETTES[k].label,
            }))}
            hint="Colours of the login page."
            configKey="proxy.auth.palette"
            disabled={disabled}
          />
          <div className="field">
            <label htmlFor="svclogin-accent">Accent colour</label>
            <span className="field-hint">Optional. Overrides the palette's main colour.</span>
            <div className="btn-row">
              <input
                id="svclogin-accent"
                type="color"
                value={validAccent ? a.accent! : palette.colors[0]}
                onChange={(e) => patch({ accent: e.target.value })}
                disabled={disabled}
              />
              <button type="button" onClick={() => patch({ accent: null })} disabled={disabled || !a.accent}>
                Use palette colour
              </button>
            </div>
            <div
              data-testid="svclogin-preview"
              style={{
                display: 'flex',
                gap: 6,
                padding: 8,
                borderRadius: 6,
                background: palette.bg,
                marginTop: 8,
              }}
            >
              {palette.colors.map((c, i) => (
                <span
                  key={i}
                  style={{
                    width: 28,
                    height: 28,
                    borderRadius: 4,
                    background: i === 0 && validAccent ? a.accent! : c,
                  }}
                />
              ))}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
