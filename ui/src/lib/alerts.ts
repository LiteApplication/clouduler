// Pure logic behind the Alerts page: grouping, routing summaries and the
// destination form <-> settings conversion. Kept out of the components so it
// can be tested without a DOM.

import type {
  AlertCatalogEntry,
  AlertDestination,
  AlertEventOverride,
  AlertRoute,
  AlertSettings,
  AlertSettingsUpdate,
  AlertSeverity,
  AlertView,
  DiscordNotify,
  DiscordNotifyLevels,
  DiscordStyle,
} from '../api/types';

export const SEVERITIES: AlertSeverity[] = ['critical', 'warning', 'info'];
export const SEVERITY_LABEL: Record<AlertSeverity, string> = { critical: 'Critical', warning: 'Warning', info: 'Info' };
export const IN_APP = 'In-app';

const RANK: Record<AlertSeverity, number> = { critical: 0, warning: 1, info: 2 };

/** Worst first, then newest. */
export function sortActive(alerts: AlertView[]): AlertView[] {
  return [...alerts].sort((a, b) => RANK[a.severity] - RANK[b.severity] || b.last_at - a.last_at);
}

/** The moment an alert is filed under: when it resolved, else when it last fired. */
export function filedAt(a: AlertView): number {
  return a.resolved_at ?? a.last_at;
}

export interface Group {
  label: string;
  items: AlertView[];
}

const startOfDay = (ms: number) => {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
};

/** Today / Yesterday / Earlier this week / Earlier, newest first. Empty groups are dropped. */
export function groupEarlier(alerts: AlertView[], now: number = Date.now()): Group[] {
  const today = startOfDay(now);
  const day = 86_400_000;
  const groups: Group[] = [
    { label: 'Today', items: [] },
    { label: 'Yesterday', items: [] },
    { label: 'Earlier this week', items: [] },
    { label: 'Earlier', items: [] },
  ];
  for (const a of [...alerts].sort((x, y) => filedAt(y) - filedAt(x))) {
    const t = filedAt(a) * 1000;
    const i = t >= today ? 0 : t >= today - day ? 1 : t >= today - 6 * day ? 2 : 3;
    groups[i].items.push(a);
  }
  return groups.filter((g) => g.items.length > 0);
}

const READ_KEY = 'clouduler.alerts.readAt';

/** Unix seconds of the last "Mark all as read". The first visit counts as
 *  reading everything that already exists, so a fresh browser is not a wall of
 *  "New" markers. */
export function loadReadAt(): number {
  try {
    const raw = localStorage.getItem(READ_KEY);
    const n = Number(raw);
    if (raw != null && Number.isFinite(n)) return n;
    const now = Math.floor(Date.now() / 1000);
    localStorage.setItem(READ_KEY, String(now));
    return now;
  } catch {
    return 0;
  }
}

export function saveReadAt(secs: number) {
  try {
    localStorage.setItem(READ_KEY, String(secs));
  } catch {
    // Storage blocked: the marker just lasts until the page is reloaded.
  }
}

/** An alert is new when it fired or got worse after the last "mark all as read". */
export function isNew(a: AlertView, readAt: number): boolean {
  return Math.max(a.last_at, a.opened_at) > readAt;
}

export function countNew(alerts: AlertView[], readAt: number): number {
  return alerts.filter((a) => isNew(a, readAt)).length;
}

export interface Sent {
  /** "Sent to In-app, homelab-alerts". */
  text: string;
  /** Destinations whose last attempt failed, with the reason when known. */
  failed: { destination: string; error?: string }[];
}

/** What happened to an alert on its way out. The latest attempt per destination wins. */
export function sentSummary(a: AlertView): Sent {
  const last = new Map<string, AlertView['deliveries'][number]>();
  for (const d of [...a.deliveries].sort((x, y) => x.at - y.at)) last.set(d.destination, d);
  const ok = [...last.values()].filter((d) => d.ok).map((d) => d.destination).sort();
  const failed = [...last.values()].filter((d) => !d.ok).map((d) => ({ destination: d.destination, error: d.error }));
  const names = [IN_APP, ...ok];
  const text =
    a.severity === 'info' && a.state !== 'active' && ok.length === 0 && failed.length === 0
      ? `${IN_APP}, in the daily digest`
      : `Sent to ${names.join(', ')}`;
  return { text, failed };
}

/** One sentence on what a destination receives. */
export function destinationReceives(d: AlertDestination): string {
  const sev = SEVERITIES.filter((s) => d.severities.includes(s)).map((s) => SEVERITY_LABEL[s].toLowerCase());
  const parts: string[] = [];
  if (sev.length === 3) parts.push('Critical, warning and info');
  else if (sev.length) parts.push(capitalize(joinList(sev)));
  if (d.digest) parts.push(sev.includes('info') ? 'info also as a daily digest' : 'info as a daily digest');
  if (d.kind === 'discord' && d.mention && d.notify.critical === 'ping' && d.severities.includes('critical')) {
    parts.push(`pings ${d.mention} on critical`);
  }
  if (d.categories?.length) parts.push(`only ${d.categories.join(', ')}`);
  if (d.stacks?.length) parts.push(`only stacks ${d.stacks.join(', ')}`);
  return parts.length ? capitalize(parts.join('; ')) : 'Nothing (no severity selected)';
}

export function joinList(xs: string[]): string {
  if (xs.length <= 1) return xs.join('');
  return `${xs.slice(0, -1).join(', ')} and ${xs[xs.length - 1]}`;
}

const capitalize = (s: string) => (s ? s[0].toUpperCase() + s.slice(1) : s);

export const KIND_LABEL = { discord: 'Discord', webhook: 'Generic', email: 'Email' } as const;

/** Who hears about each severity, derived from the saved destinations. */
export function routingSummary(settings: AlertSettings): Record<AlertSeverity, string[]> {
  const out: Record<AlertSeverity, string[]> = { critical: [IN_APP], warning: [IN_APP], info: [IN_APP] };
  if (!settings.enabled) return out;
  for (const d of settings.destinations) {
    if (!d.enabled) continue;
    for (const s of SEVERITIES) {
      if (s === 'info') {
        if (d.severities.includes('info')) out.info.push(d.name);
        else if (d.digest) out.info.push(`${d.name} (digest)`);
      } else if (d.severities.includes(s)) out[s].push(d.name);
    }
  }
  return out;
}

export type RouteKind = 'default' | 'in_app' | 'muted' | 'destinations';

export function routeKind(o: AlertEventOverride | null | undefined): RouteKind {
  const r = o?.route;
  if (!r) return 'default';
  if (r === 'muted') return 'muted';
  if (r === 'in_app') return 'in_app';
  return 'destinations';
}

export function routeDestinations(o: AlertEventOverride | null | undefined): string[] {
  const r = o?.route;
  return r && typeof r === 'object' ? r.destinations : [];
}

/** The "Goes to" cell. */
export function routeLabel(e: AlertCatalogEntry): string {
  switch (routeKind(e.override)) {
    case 'muted':
      return 'Muted';
    case 'in_app':
      return 'In-app only';
    case 'destinations':
      return [IN_APP, ...routeDestinations(e.override)].join(', ');
    default:
      return e.routes.length ? e.routes.join(', ') : IN_APP;
  }
}

export function isCustom(e: AlertCatalogEntry): boolean {
  return !!e.override && (e.override.severity != null || e.override.route != null);
}

/** Build an override from the editor's choices; `undefined` means "no override at all". */
export function buildOverride(
  severity: AlertSeverity | 'default',
  kind: RouteKind,
  destinations: string[],
): AlertEventOverride | undefined {
  const o: AlertEventOverride = {};
  if (severity !== 'default') o.severity = severity;
  const route: AlertRoute | undefined =
    kind === 'muted' ? 'muted' : kind === 'in_app' ? 'in_app' : kind === 'destinations' ? { destinations } : undefined;
  if (route) o.route = route;
  return o.severity || o.route ? o : undefined;
}

export function withOverride(settings: AlertSettings, key: string, o: AlertEventOverride | undefined): AlertSettings {
  const events = { ...settings.events };
  if (o) events[key] = o;
  else delete events[key];
  return { ...settings, events };
}

/** The events matching a category and severity chip. */
export function filterCatalog(
  catalog: AlertCatalogEntry[],
  category: string,
  severity: AlertSeverity | 'all',
): AlertCatalogEntry[] {
  return catalog.filter(
    (e) => (category === 'all' || e.category === category) && (severity === 'all' || e.effective_severity === severity),
  );
}

// ---------------------------------------------------------------- humantime

const UNITS: Record<string, number> = {
  s: 1, sec: 1, secs: 1, second: 1, seconds: 1,
  m: 60, min: 60, mins: 60, minute: 60, minutes: 60,
  h: 3600, hr: 3600, hrs: 3600, hour: 3600, hours: 3600,
  d: 86_400, day: 86_400, days: 86_400,
  w: 604_800, week: 604_800, weeks: 604_800,
};

/** Seconds in a humantime string such as `5m`, `1h 30m` or `30days`; null when it is not one. */
export function parseDuration(text: string): number | null {
  const t = text.trim();
  if (!t) return null;
  const re = /(\d+)\s*([a-z]+)/gy;
  let total = 0;
  let pos = 0;
  for (;;) {
    while (t[pos] === ' ') pos++;
    if (pos >= t.length) break;
    re.lastIndex = pos;
    const m = re.exec(t);
    const unit = m ? UNITS[m[2]] : undefined;
    if (!m || unit === undefined) return null;
    total += Number(m[1]) * unit;
    pos = re.lastIndex;
  }
  return total;
}

export function validTime(text: string): boolean {
  const m = /^(\d{2}):(\d{2})$/.exec(text.trim());
  return !!m && Number(m[1]) < 24 && Number(m[2]) < 60;
}

// ------------------------------------------------------------ destination form

export type DestKind = AlertDestination['kind'];

export interface DestForm {
  /** The saved name this edits, null when adding. */
  original: string | null;
  kind: DestKind;
  name: string;
  enabled: boolean;
  /** Plain text; blank while editing means "keep the saved one". */
  url: string;
  auth: string;
  clearAuth: boolean;
  style: DiscordStyle;
  mention: string;
  notify: DiscordNotifyLevels;
  buttons: boolean;
  username: string;
  /** One per line or comma separated. */
  to: string;
  severities: AlertSeverity[];
  digest: boolean;
  categories: string[];
  stacks: string;
}

export const DEFAULT_NOTIFY: DiscordNotifyLevels = { critical: 'ping', warning: 'normal', info: 'silent', resolved: 'silent' };

export function newForm(kind: DestKind): DestForm {
  return {
    original: null,
    kind,
    name: '',
    enabled: true,
    url: '',
    auth: '',
    clearAuth: false,
    style: 'embed',
    mention: '',
    notify: { ...DEFAULT_NOTIFY },
    buttons: true,
    username: '',
    to: '',
    severities: ['critical', 'warning'],
    digest: false,
    categories: [],
    stacks: '',
  };
}

export function formFrom(d: AlertDestination): DestForm {
  const f = newForm(d.kind);
  f.original = d.name;
  f.name = d.name;
  f.enabled = d.enabled;
  f.severities = [...d.severities];
  f.digest = d.digest;
  f.categories = [...(d.categories ?? [])];
  f.stacks = (d.stacks ?? []).join(', ');
  if (d.kind === 'discord') {
    f.style = d.style;
    f.mention = d.mention ?? '';
    f.notify = { ...d.notify };
    f.buttons = d.buttons;
    f.username = d.username ?? '';
  } else if (d.kind === 'email') {
    f.to = d.to.join('\n');
  }
  return f;
}

export const splitList = (text: string): string[] =>
  text
    .split(/[\s,;]+/)
    .map((s) => s.trim())
    .filter(Boolean);

export function formToDestination(f: DestForm): AlertDestination {
  const base = {
    name: f.name.trim(),
    enabled: f.enabled,
    severities: SEVERITIES.filter((s) => f.severities.includes(s)),
    digest: f.digest,
    ...(f.categories.length ? { categories: f.categories } : {}),
    ...(splitList(f.stacks).length ? { stacks: splitList(f.stacks) } : {}),
  };
  if (f.kind === 'discord') {
    return {
      ...base,
      kind: 'discord',
      style: f.style,
      ...(f.mention.trim() ? { mention: f.mention.trim() } : {}),
      notify: f.notify,
      buttons: f.buttons,
      ...(f.username.trim() ? { username: f.username.trim() } : {}),
    };
  }
  if (f.kind === 'email') return { ...base, kind: 'email', to: splitList(f.to) };
  return { ...base, kind: 'webhook' };
}

const NAME_RE = /^[a-z0-9-]{1,40}$/;
const DISCORD_URL = /^https:\/\/(?:(?:ptb|canary)\.)?discord(?:app)?\.com\/api(?:\/v\d+)?\/webhooks\/\d+\/[\w-]+/;
const EMAIL_RE = /^[^\s@<>]+@[^\s@<>]+\.[^\s@<>]+$/;
const MENTION_RE = /^(@everyone|@here|<@&?!?\d+>)$/;

export type FormErrors = Partial<Record<'name' | 'url' | 'to' | 'mention' | 'severities', string>>;

/** Client-side checks that mirror the server's, for instant feedback. `taken` are the other destinations' names. */
export function validateForm(f: DestForm, taken: string[], savedUrl: boolean): FormErrors {
  const e: FormErrors = {};
  const name = f.name.trim();
  if (!NAME_RE.test(name)) e.name = 'Use 1 to 40 lowercase letters, digits or dashes.';
  else if (taken.includes(name) && name !== f.original) e.name = 'Another destination already has this name.';
  if (f.kind !== 'email') {
    const url = f.url.trim();
    if (!url && !(f.original && savedUrl)) e.url = 'Paste the webhook URL.';
    else if (url && f.kind === 'discord' && !DISCORD_URL.test(url)) e.url = 'That does not look like a Discord webhook URL.';
    else if (url && f.kind === 'webhook' && !/^https?:\/\/[^\s]+$/.test(url)) e.url = 'The URL must start with http:// or https://.';
  } else {
    const to = splitList(f.to);
    if (to.length === 0) e.to = 'Add at least one recipient.';
    else if (to.some((a) => !EMAIL_RE.test(a))) e.to = `"${to.find((a) => !EMAIL_RE.test(a))}" is not an email address.`;
  }
  if (f.kind === 'discord' && f.mention.trim() && !MENTION_RE.test(f.mention.trim())) {
    e.mention = 'Use @here, @everyone, <@&roleId> or <@userId>.';
  }
  if (f.severities.length === 0 && !f.digest) e.severities = 'Pick at least one severity, or the daily digest.';
  return e;
}

/** The secret edits a form implies: blank keeps what is saved. */
export function secretEdits(f: DestForm): { url?: string; auth?: string | null } | null {
  const out: { url?: string; auth?: string | null } = {};
  if (f.kind !== 'email' && f.url.trim()) out.url = f.url.trim();
  if (f.kind === 'webhook') {
    if (f.clearAuth) out.auth = null;
    else if (f.auth.trim()) out.auth = f.auth.trim();
  }
  return Object.keys(out).length ? out : null;
}

/** Settings after adding or replacing a destination. */
export function upsertDestination(settings: AlertSettings, original: string | null, dest: AlertDestination): AlertSettings {
  const list = [...settings.destinations];
  const i = original == null ? -1 : list.findIndex((d) => d.name === original);
  if (i >= 0) list[i] = dest;
  else list.push(dest);
  return { ...settings, destinations: list };
}

/** Settings without a destination, and without it in any event's route. */
export function removeDestination(settings: AlertSettings, name: string): AlertSettings {
  const events: AlertSettings['events'] = {};
  for (const [k, o] of Object.entries(settings.events)) {
    const route = o.route;
    if (route && typeof route === 'object') {
      const rest = route.destinations.filter((n) => n !== name);
      const next = buildOverride(o.severity ?? 'default', rest.length ? 'destinations' : 'default', rest);
      if (next) events[k] = next;
    } else events[k] = o;
  }
  return { ...settings, destinations: settings.destinations.filter((d) => d.name !== name), events };
}

/** The payload for `PUT /api/alerts/settings`. */
export function saveBody(
  settings: AlertSettings,
  secrets?: AlertSettingsUpdate['secrets'],
  smtpPassword?: string | null,
): AlertSettingsUpdate {
  const body: AlertSettingsUpdate = { settings };
  if (secrets && Object.keys(secrets).length) body.secrets = secrets;
  if (smtpPassword !== undefined) body.smtp_password = smtpPassword;
  return body;
}

export function describeNotify(n: DiscordNotify): string {
  return n === 'ping' ? 'Ping' : n === 'normal' ? 'Normal' : 'Silent';
}

/** Destinations an alert-count badge cares about: failing ones. */
export function failingCount(health: Record<string, { ok: boolean | null }>, settings: AlertSettings): number {
  return settings.destinations.filter((d) => d.enabled && health[d.name]?.ok === false).length;
}
