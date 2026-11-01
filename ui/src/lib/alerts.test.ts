import { describe, expect, it } from 'vitest';
import type { AlertCatalogEntry, AlertDestination, AlertSettings, AlertView } from '../api/types';
import {
  buildOverride,
  countNew,
  destinationReceives,
  filterCatalog,
  formFrom,
  formToDestination,
  groupEarlier,
  isNew,
  newForm,
  parseDuration,
  removeDestination,
  routeLabel,
  routingSummary,
  secretEdits,
  sentSummary,
  sortActive,
  upsertDestination,
  validateForm,
  validTime,
} from './alerts';

const NOW = new Date(2026, 6, 15, 12, 0, 0).getTime(); // a Wednesday, local time
const at = (daysAgo: number, hour = 9) => new Date(2026, 6, 15 - daysAgo, hour, 0, 0).getTime() / 1000;

function alert(over: Partial<AlertView> = {}): AlertView {
  return {
    id: 'a', event: 'instance.crash_loop', category: 'workloads', severity: 'warning', state: 'active',
    title: 't', detail: 'd', subject: 's', opened_at: at(0), last_at: at(0), count: 1, links: [], excerpt: [], deliveries: [],
    ...over,
  };
}

function settings(over: Partial<AlertSettings> = {}): AlertSettings {
  return {
    enabled: true, warning_debounce: '5m', digest_at: '09:00', renotify_on_escalation: true, notify_resolved: true,
    retention: '30days', destinations: [], events: {},
    thresholds: { disk_warning_percent: 90, disk_critical_percent: 95, memory_percent: 92, memory_for: '10m', cert_warning_days: 14, cert_critical_days: 7 },
    ...over,
  };
}

const discord = (over: Partial<AlertDestination> = {}): AlertDestination =>
  ({
    name: 'home', enabled: true, kind: 'discord', style: 'embed', notify: { critical: 'ping', warning: 'normal', info: 'silent', resolved: 'silent' },
    buttons: true, edit_resolved: false, severities: ['critical', 'warning'], digest: false, ...over,
  }) as AlertDestination;

describe('active order', () => {
  it('puts the worst first, then the newest', () => {
    const out = sortActive([
      alert({ id: 'w-old', severity: 'warning', last_at: 1 }),
      alert({ id: 'c-old', severity: 'critical', last_at: 1 }),
      alert({ id: 'w-new', severity: 'warning', last_at: 9 }),
      alert({ id: 'i', severity: 'info', last_at: 99 }),
    ]);
    expect(out.map((a) => a.id)).toEqual(['c-old', 'w-new', 'w-old', 'i']);
  });
});

describe('groupEarlier', () => {
  it('buckets by local day, newest first, and drops empty groups', () => {
    const groups = groupEarlier(
      [
        alert({ id: 'today', state: 'resolved', resolved_at: at(0, 8) }),
        alert({ id: 'yday', last_at: at(1, 23) }),
        alert({ id: 'week', last_at: at(4) }),
        alert({ id: 'old', last_at: at(20) }),
        alert({ id: 'today2', last_at: at(0, 11) }),
      ],
      NOW,
    );
    expect(groups.map((g) => g.label)).toEqual(['Today', 'Yesterday', 'Earlier this week', 'Earlier']);
    expect(groups[0].items.map((a) => a.id)).toEqual(['today2', 'today']);
    expect(groupEarlier([alert({ last_at: at(20) })], NOW).map((g) => g.label)).toEqual(['Earlier']);
    expect(groupEarlier([], NOW)).toEqual([]);
  });
});

describe('read markers', () => {
  it('is new until marked read, and again when it fires after', () => {
    const a = alert({ last_at: 100, opened_at: 50 });
    expect(isNew(a, 0)).toBe(true);
    expect(isNew(a, 100)).toBe(false);
    expect(isNew({ ...a, last_at: 101 }, 100)).toBe(true);
    expect(countNew([a, { ...a, last_at: 10, opened_at: 10 }], 20)).toBe(1);
  });
});

describe('sentSummary', () => {
  it('lists successes and failures, the latest attempt per destination wins', () => {
    const a = alert({
      deliveries: [
        { destination: 'pager', at: 1, ok: false, error: 'timed out', attempts: 5 },
        { destination: 'home', at: 2, ok: true, attempts: 1 },
        { destination: 'pager', at: 3, ok: true, attempts: 1 },
        { destination: 'mail', at: 4, ok: false, error: 'refused', attempts: 5 },
      ],
    });
    const s = sentSummary(a);
    expect(s.text).toBe('Sent to In-app, home, pager');
    expect(s.failed).toEqual([{ destination: 'mail', error: 'refused' }]);
  });
  it('says digest for quiet info alerts', () => {
    expect(sentSummary(alert({ severity: 'info', state: 'event' })).text).toBe('In-app, in the daily digest');
    expect(sentSummary(alert()).text).toBe('Sent to In-app');
  });
});

describe('routing summary', () => {
  it('derives who gets what from the destinations', () => {
    const s = settings({
      destinations: [
        discord({ name: 'a' }),
        discord({ name: 'b', severities: ['critical'], digest: true }),
        discord({ name: 'off', enabled: false }),
        discord({ name: 'c', severities: [], digest: true }),
      ],
    });
    expect(routingSummary(s)).toEqual({
      critical: ['In-app', 'a', 'b'],
      warning: ['In-app', 'a'],
      info: ['In-app', 'b (digest)', 'c (digest)'],
    });
  });
  it('sends nothing out when alerts are switched off', () => {
    expect(routingSummary(settings({ enabled: false, destinations: [discord()] })).critical).toEqual(['In-app']);
  });
});

describe('destinationReceives', () => {
  it('mentions the ping and the digest', () => {
    expect(destinationReceives(discord({ mention: '@oncall' }))).toBe('Critical and warning; pings @oncall on critical');
    expect(destinationReceives(discord({ severities: ['critical'], digest: true }))).toBe('Critical; info as a daily digest');
    expect(destinationReceives(discord({ severities: [], digest: false }))).toBe('Nothing (no severity selected)');
  });
});

describe('overrides', () => {
  it('builds nothing when everything is default', () => {
    expect(buildOverride('default', 'default', [])).toBeUndefined();
    expect(buildOverride('critical', 'default', [])).toEqual({ severity: 'critical' });
    expect(buildOverride('default', 'muted', [])).toEqual({ route: 'muted' });
    expect(buildOverride('warning', 'destinations', ['a'])).toEqual({ severity: 'warning', route: { destinations: ['a'] } });
  });
  it('labels routes', () => {
    const e: AlertCatalogEntry = {
      key: 'k', category: 'c', title: 't', severity: 'info', note: '', stateful: false, stack_scoped: false,
      override: null, effective_severity: 'info', routes: ['in-app', 'digest'],
    };
    expect(routeLabel(e)).toBe('in-app, digest');
    expect(routeLabel({ ...e, override: { route: 'muted' } })).toBe('Muted');
    expect(routeLabel({ ...e, override: { route: 'in_app' } })).toBe('In-app only');
    expect(routeLabel({ ...e, override: { route: { destinations: ['x', 'y'] } } })).toBe('In-app, x, y');
  });
  it('filters the catalog by category and effective severity', () => {
    const mk = (key: string, category: string, eff: AlertCatalogEntry['effective_severity']): AlertCatalogEntry => ({
      key, category, title: key, severity: 'info', note: '', stateful: false, stack_scoped: false, override: null, effective_severity: eff, routes: [],
    });
    const cat = [mk('a', 'x', 'info'), mk('b', 'x', 'critical'), mk('c', 'y', 'critical')];
    expect(filterCatalog(cat, 'x', 'all').map((e) => e.key)).toEqual(['a', 'b']);
    expect(filterCatalog(cat, 'all', 'critical').map((e) => e.key)).toEqual(['b', 'c']);
  });
});

describe('destination form', () => {
  it('round-trips a discord destination', () => {
    const d = discord({ mention: '<@&1>', username: 'Piosny', categories: ['nodes'], stacks: ['media'] });
    const f = formFrom(d);
    expect(f.original).toBe('home');
    expect(f.stacks).toBe('media');
    expect(formToDestination(f)).toEqual(d);
  });
  it('trims and drops empty optional fields', () => {
    const f = { ...newForm('discord'), name: ' home ', mention: ' ', username: '', stacks: ' ' };
    const d = formToDestination(f);
    expect(d).not.toHaveProperty('mention');
    expect(d).not.toHaveProperty('username');
    expect(d).not.toHaveProperty('stacks');
    expect(d.name).toBe('home');
  });
  it('splits email recipients on commas, spaces and lines', () => {
    const f = { ...newForm('email'), name: 'mail', to: 'a@b.co, c@d.co\ne@f.co' };
    expect(formToDestination(f)).toMatchObject({ kind: 'email', to: ['a@b.co', 'c@d.co', 'e@f.co'] });
  });
  it('validates like the server', () => {
    const ok = { ...newForm('discord'), name: 'home', url: 'https://discord.com/api/webhooks/123/abc-DEF_1' };
    expect(validateForm(ok, [], false)).toEqual({});
    expect(validateForm({ ...ok, name: 'Bad Name' }, [], false).name).toBeTruthy();
    expect(validateForm(ok, ['home'], false).name).toMatch(/already/);
    expect(validateForm({ ...ok, url: 'https://example.com/x' }, [], false).url).toMatch(/Discord/);
    expect(validateForm({ ...ok, url: '' }, [], false).url).toBeTruthy();
    expect(validateForm({ ...ok, mention: 'oncall' }, [], false).mention).toBeTruthy();
    expect(validateForm({ ...ok, mention: '<@&123>' }, [], false).mention).toBeUndefined();
    expect(validateForm({ ...ok, severities: [], digest: false }, [], false).severities).toBeTruthy();
    // Editing keeps the saved URL when the field stays blank.
    expect(validateForm({ ...ok, url: '', original: 'home' }, ['home'], true)).toEqual({});
    const hook = { ...newForm('webhook'), name: 'hook', url: 'ftp://x' };
    expect(validateForm(hook, [], false).url).toBeTruthy();
    expect(validateForm({ ...newForm('email'), name: 'm', to: 'nope' }, [], false).to).toMatch(/nope/);
    expect(validateForm({ ...newForm('email'), name: 'm', to: '' }, [], false).to).toBeTruthy();
  });
  it('only sends secrets that were typed', () => {
    const f = { ...newForm('webhook'), name: 'h' };
    expect(secretEdits(f)).toBeNull();
    expect(secretEdits({ ...f, url: ' https://x.test ', auth: 'Bearer t' })).toEqual({ url: 'https://x.test', auth: 'Bearer t' });
    expect(secretEdits({ ...f, clearAuth: true, auth: 'ignored' })).toEqual({ auth: null });
    expect(secretEdits({ ...newForm('email'), url: 'x' })).toBeNull();
  });
});

describe('settings edits', () => {
  it('upserts by original name', () => {
    const s = settings({ destinations: [discord({ name: 'a' }), discord({ name: 'b' })] });
    expect(upsertDestination(s, null, discord({ name: 'c' })).destinations.map((d) => d.name)).toEqual(['a', 'b', 'c']);
    expect(upsertDestination(s, 'a', discord({ name: 'a', enabled: false })).destinations[0].enabled).toBe(false);
  });
  it('removing a destination also cleans event routes, and drops routes that become empty', () => {
    const s = settings({
      destinations: [discord({ name: 'a' }), discord({ name: 'b' })],
      events: {
        one: { route: { destinations: ['a', 'b'] } },
        two: { severity: 'critical', route: { destinations: ['a'] } },
        three: { route: 'muted' },
      },
    });
    const out = removeDestination(s, 'a');
    expect(out.destinations.map((d) => d.name)).toEqual(['b']);
    expect(out.events).toEqual({ one: { route: { destinations: ['b'] } }, two: { severity: 'critical' }, three: { route: 'muted' } });
  });
});

describe('durations and times', () => {
  it('parses humantime', () => {
    expect(parseDuration('5m')).toBe(300);
    expect(parseDuration('1h 30m')).toBe(5400);
    expect(parseDuration('30days')).toBe(30 * 86400);
    expect(parseDuration('')).toBeNull();
    expect(parseDuration('five')).toBeNull();
    expect(parseDuration('5 parsecs')).toBeNull();
  });
  it('checks HH:MM', () => {
    expect(validTime('09:00')).toBe(true);
    expect(validTime('24:00')).toBe(false);
    expect(validTime('9:00')).toBe(false);
  });
});
