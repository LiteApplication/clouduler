import { CompletionContext } from '@codemirror/autocomplete';
import { EditorState } from '@codemirror/state';
import { describe, expect, it } from 'vitest';
import { collectImages, emptyKnownNames, stackSpecCompletionSource, type KnownNames } from './stackSchema';

const known: KnownNames = {
  networks: ['wan', 'internal'],
  drives: ['fast', 'bulk'],
  nodes: ['n1', 'n2'],
  instances: [],
  secrets: ['api-key'],
  configs: ['nginx-conf'],
  volumes: ['fast/gitlab-data', 'fast/gitlab-config'],
  images: ['nginx:1.25-alpine', 'postgres:16'],
};

/** Runs the completion source with the cursor placed at the first `|` in
 *  `docWithCursor`, returning the suggested labels (or null for no result). */
function complete(docWithCursor: string, names: KnownNames = known): string[] | null {
  const pos = docWithCursor.indexOf('|');
  if (pos < 0) throw new Error('docWithCursor must contain a | marking the cursor');
  const doc = docWithCursor.slice(0, pos) + docWithCursor.slice(pos + 1);
  const state = EditorState.create({ doc });
  const context = new CompletionContext(state, pos, true);
  const result = stackSpecCompletionSource(() => names)(context);
  return result ? result.options.map((o) => o.label) : null;
}

describe('stackSpecCompletionSource', () => {
  it('suggests root-level schema keys', () => {
    expect(complete('|')).toEqual(
      expect.arrayContaining(['version', 'services', 'networks', 'volumes', 'configs', 'secrets', 'x-clouduler']),
    );
  });

  it('resolves keys after the first one in a list-of-maps item (regression: dash-vs-sibling indent)', () => {
    // Before the indent fix, `host` (the dash line's own key) never got
    // popped by its same-item siblings, so `network:`/`port:` here resolved
    // against a bogus path and got no suggestions at all.
    const doc = [
      'services:',
      '  web:',
      '    image: nginx',
      '    x-clouduler:',
      '      proxy:',
      '        - host: site',
      '          |',
    ].join('\n');
    expect(complete(doc)).toEqual(
      expect.arrayContaining(['network', 'port', 'path_prefix', 'tls', 'middlewares']),
    );
  });

  it('suggests same-file service names for depends_on', () => {
    const doc = [
      'services:',
      '  web:',
      '    image: nginx',
      '    depends_on:',
      '      - |',
      '  db:',
      '    image: postgres',
    ].join('\n');
    expect(complete(doc)).toEqual(['web', 'db']);
  });

  it('suggests same-file service names for colocate', () => {
    const doc = [
      'services:',
      '  web:',
      '    image: nginx',
      '    x-clouduler:',
      '      colocate:',
      '        - |',
      '  cache:',
      '    image: redis',
    ].join('\n');
    expect(complete(doc)).toEqual(['web', 'cache']);
  });

  it('suggests cluster-wide and same-file images for image:', () => {
    const doc = ['services:', '  web:', '    image: |', '  worker:', '    image: myapp:v3'].join('\n');
    expect(complete(doc)).toEqual(expect.arrayContaining(['myapp:v3', 'nginx:1.25-alpine', 'postgres:16']));
  });

  it('suggests known drive/volume ids for a service volume mount', () => {
    const doc = ['services:', '  web:', '    image: nginx', '    volumes:', '      - |'].join('\n');
    expect(complete(doc)).toEqual(['fast/gitlab-data:', 'fast/gitlab-config:']);
  });

  it('falls back to bare drive names when no volumes are known yet', () => {
    const doc = ['services:', '  web:', '    image: nginx', '    volumes:', '      - |'].join('\n');
    expect(complete(doc, { ...known, volumes: [] })).toEqual(['fast/', 'bulk/']);
  });

  it('suggests cluster secrets for a service secret source', () => {
    const doc = ['services:', '  web:', '    image: nginx', '    secrets:', '      - source: |'].join('\n');
    expect(complete(doc)).toEqual(['api-key']);
  });

  it('suggests cluster networks as new keys under the top-level networks map', () => {
    const doc = ['services:', '  web:', '    image: nginx', 'networks:', '  |'].join('\n');
    expect(complete(doc)).toEqual(['wan', 'internal']);
  });

  it('suggests bare and configured middleware names', () => {
    const doc = [
      'services:',
      '  web:',
      '    image: nginx',
      '    x-clouduler:',
      '      proxy:',
      '        - host: site',
      '          network: wan',
      '          port: 80',
      '          middlewares:',
      '            - |',
    ].join('\n');
    expect(complete(doc)).toEqual(
      expect.arrayContaining(['redirect_https', 'strip_prefix', 'authenticated', 'basic_auth:', 'headers:']),
    );
  });

  it('suggests boolean literals for known boolean keys', () => {
    const doc = ['services:', '  web:', '    image: nginx', '    x-clouduler:', '      proxy:', '        - host: site', '          tls: |'].join('\n');
    expect(complete(doc)).toEqual(['true', 'false']);
  });

  it('suggests restart policy values', () => {
    const doc = ['services:', '  web:', '    image: nginx', '    restart: |'].join('\n');
    expect(complete(doc)).toEqual(['no', 'always', 'on-failure', 'unless-stopped']);
  });
});

describe('actions', () => {
  const doc = [
    'services:',
    '  db:',
    '    image: postgres',
    '    x-clouduler:',
    '      actions:',
    '        Reset WAL:',
    '          cmd: pg_resetwal',
    '        Checkpoint:',
    '          cmd: x',
    '      volume_sync:',
    '        data:',
    '          before: |',
  ].join('\n');

  it('suggests action keys, including inside a spaced action name', () => {
    const lines = doc.split('\n');
    expect(complete([...lines.slice(0, 7), '          |'].join('\n'))).toEqual(
      expect.arrayContaining(['cmd', 'user', 'timeout', 'confirm', 'show_in_ui']),
    );
  });

  it('suggests booleans for confirm and show_in_ui', () => {
    expect(complete('services:\n  a:\n    x-clouduler:\n      actions:\n        X:\n          show_in_ui: |')).toEqual([
      'true',
      'false',
    ]);
  });

  it('completes before/after with the service\'s action names', () => {
    expect(complete(doc)).toEqual(['Reset WAL', 'Checkpoint']);
  });

  it('suggests before/after under volume_sync', () => {
    expect(
      complete('services:\n  a:\n    x-clouduler:\n      volume_sync:\n        data:\n          |'),
    ).toEqual(expect.arrayContaining(['interval', 'enabled', 'before', 'after']));
  });
});

describe('collectImages', () => {
  it('collects distinct image values across multiple specs', () => {
    const a = 'services:\n  web:\n    image: nginx:1.25\n';
    const b = 'services:\n  worker:\n    image: nginx:1.25\n  db:\n    image: postgres:16\n';
    expect(collectImages([a, b])).toEqual(['nginx:1.25', 'postgres:16']);
  });

  it('returns nothing for a spec with no services', () => {
    expect(collectImages([''])).toEqual([]);
  });
});

it('emptyKnownNames has empty arrays for every field', () => {
  expect(Object.values(emptyKnownNames).every((v) => Array.isArray(v) && v.length === 0)).toBe(true);
});
