import { describe, expect, it } from 'vitest';
import { applyEnvImport, declaredSecrets, defaultSecretName, detectEnvFiles, looksSensitive, secretNameError } from './envImport';

const SPEC = `# my stack
services:
  web:
    image: x   # pinned
    env_file:
      - .env
      - ./b.env
    environment:
      A: explicit
  db:
    image: y
    env_file: db.env
`;

describe('detectEnvFiles', () => {
  it('finds string and list forms', () => {
    expect(detectEnvFiles(SPEC)).toEqual([
      { service: 'web', files: ['.env', './b.env'] },
      { service: 'db', files: ['db.env'] },
    ]);
  });
  it('returns nothing for broken yaml or no env_file', () => {
    expect(detectEnvFiles('a: [')).toEqual([]);
    expect(detectEnvFiles('services:\n  a:\n    image: x\n')).toEqual([]);
  });
});

describe('heuristics', () => {
  it('flags sensitive names', () => {
    for (const k of ['DB_PASSWORD', 'API_KEY', 'JWT_SECRET', 'AUTH_TOKEN', 'SALT', 'PRIVATE_KEY', 'AWS_CREDENTIALS']) expect(looksSensitive(k)).toBe(true);
    for (const k of ['PORT', 'LOG_LEVEL', 'DB_HOST', 'PUBLIC_KEY']) expect(looksSensitive(k)).toBe(false);
  });
  it('builds default names and validates them', () => {
    expect(defaultSecretName('web', 'DB_PASSWORD')).toBe('web-db-password');
    expect(secretNameError('web-db-password')).toBeNull();
    expect(secretNameError('')).not.toBeNull();
    expect(secretNameError('.x')).not.toBeNull();
    expect(secretNameError('a b')).not.toBeNull();
  });
});

describe('applyEnvImport', () => {
  it('moves entries, keeps comments and explicit values', () => {
    const r = applyEnvImport(SPEC, {
      service: 'web',
      plain: [{ key: 'A', value: 'file' }, { key: 'PORT', value: '8080' }, { key: 'FLAG', value: 'true' }],
      secrets: [{ key: 'DB_PASSWORD', name: 'web-db-password', value: 'p w' }],
    });
    expect(r.skipped).toEqual(['A']);
    expect(r.secretValues).toEqual({ 'web-db-password': 'p w' });
    expect(r.spec).toContain('# my stack');
    expect(r.spec).toContain('# pinned');
    expect(r.spec).toContain('A: explicit');
    expect(r.spec).toContain('PORT: "8080"');
    expect(r.spec).toContain('FLAG: "true"');
    expect(r.spec).toContain('file: ./secrets/web-db-password');
    expect(r.spec).toContain('source: web-db-password');
    expect(r.spec).toContain('env: DB_PASSWORD');
    expect(r.spec).toContain('env_file: db.env');
    expect(declaredSecrets(r.spec)).toEqual(['web-db-password']);
    expect(detectEnvFiles(r.spec).map((x) => x.service)).toEqual(['db']);
  });
  it('respects list-form environment and creates a map when missing', () => {
    const list = applyEnvImport('services:\n  a:\n    image: x\n    env_file: .env\n    environment:\n      - A=1\n', {
      service: 'a',
      plain: [{ key: 'A', value: '2' }, { key: 'B', value: '3' }],
      secrets: [],
    });
    expect(list.spec).toContain('- B=3');
    expect(list.skipped).toEqual(['A']);
    const map = applyEnvImport('services:\n  a:\n    image: x\n    env_file: .env\n', { service: 'a', plain: [{ key: 'B', value: 'x y' }], secrets: [] });
    expect(map.spec).toContain('environment:\n      B: x y');
    expect(map.spec).not.toContain('env_file');
  });
  it('appends to existing secrets sections', () => {
    const r = applyEnvImport('secrets:\n  s1:\n    file: ./s1\nservices:\n  a:\n    image: x\n    env_file: .env\n    secrets:\n      - s1\n', {
      service: 'a',
      plain: [],
      secrets: [{ key: 'T', name: 's2', value: 'v' }],
    });
    expect(declaredSecrets(r.spec)).toEqual(['s1', 's2']);
    expect(r.spec).toContain('- s1');
    expect(r.spec).toContain('source: s2');
  });
  it('errors on a missing service or bad yaml', () => {
    expect(() => applyEnvImport(SPEC, { service: 'nope', plain: [], secrets: [] })).toThrow();
    expect(() => applyEnvImport('a: [', { service: 'a', plain: [], secrets: [] })).toThrow();
  });
});
