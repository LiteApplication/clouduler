// Turns a service's `env_file` into plain stack content: finds the env_file
// references in a spec, guesses which variables are sensitive, and rewrites the
// spec (keeping comments) with the entries moved into `environment` or into
// bundled secrets exposed as environment variables.

import { isMap, isScalar, isSeq, parseDocument, YAMLMap, YAMLSeq, type Document } from 'yaml';

export interface EnvFileRef {
  service: string;
  files: string[];
}

/** Services whose `env_file` is set, with the paths it lists. */
export function detectEnvFiles(spec: string): EnvFileRef[] {
  if (!spec.includes('env_file')) return [];
  const doc = parseDocument(spec);
  if (doc.errors.length) return [];
  const services = doc.get('services', true);
  if (!isMap(services)) return [];
  const out: EnvFileRef[] = [];
  for (const pair of services.items) {
    const svc = pair.value;
    if (!isMap(svc) || !svc.has('env_file')) continue;
    const ef = svc.get('env_file', true);
    const files: string[] = [];
    if (isScalar(ef) && ef.value != null) files.push(String(ef.value));
    else if (isSeq(ef)) for (const f of ef.items) if (isScalar(f) && f.value != null) files.push(String(f.value));
    out.push({ service: String(isScalar(pair.key) ? pair.key.value : pair.key), files });
  }
  return out;
}

const SENSITIVE = /(PASSWORD|PASSWD|PASS|SECRET|TOKEN|KEY|PRIVATE|CREDENTIAL|SALT|AUTH|CERT|DSN)/i;
/** Names that contain "key" but are identifiers, not credentials. */
const NOT_SENSITIVE = /(KEYBOARD|KEYMAP|KEYSPACE|KEYWORD|PUBLIC_KEY|KEY_ID|KEYID|KEY_NAME|KEY_PATH|KEY_FILE|KEYFILE)$/i;

/** Heuristic for pre-selecting Secret. The user always has the final say. */
export function looksSensitive(key: string): boolean {
  if (NOT_SENSITIVE.test(key)) return false;
  return SENSITIVE.test(key);
}

/** `<service>-<key lowercase, _ to ->`. */
export function defaultSecretName(service: string, key: string): string {
  return `${service}-${key.toLowerCase().replace(/_/g, '-')}`;
}

/** Same rules the cluster applies to secret names. Null when fine. */
export function secretNameError(name: string): string | null {
  if (!name) return 'Name is required.';
  if (name.length > 253) return 'At most 253 characters.';
  if (!/^[A-Za-z0-9_.-]+$/.test(name)) return 'Only letters, digits, dot, dash and underscore.';
  if (name.startsWith('.')) return 'Must not start with a dot.';
  return null;
}

export interface EnvImportInput {
  service: string;
  plain: { key: string; value: string }[];
  secrets: { key: string; name: string; value: string }[];
}

export interface EnvImportResult {
  spec: string;
  /** Keys left out because the service already defines them. */
  skipped: string[];
  /** Secret names bundled by this import: name -> value. */
  secretValues: Record<string, string>;
}

function envHas(env: unknown, key: string): boolean {
  if (isMap(env)) return env.has(key);
  if (isSeq(env)) {
    return env.items.some((it) => {
      const s = isScalar(it) ? String(it.value) : '';
      return s === key || s.startsWith(`${key}=`);
    });
  }
  return false;
}

function secretEnvHas(list: unknown, key: string): boolean {
  if (!isSeq(list)) return false;
  return list.items.some((it) => isMap(it) && isScalar(it.get('env', true)) && String(it.get('env')) === key);
}

/** Rewrites the spec. Throws an Error with a readable message when the spec
 *  cannot be parsed or the service is missing. */
export function applyEnvImport(spec: string, input: EnvImportInput): EnvImportResult {
  const doc: Document = parseDocument(spec);
  if (doc.errors.length) throw new Error(`The spec has a YAML error: ${doc.errors[0].message}`);
  const svc = doc.getIn(['services', input.service], true);
  if (!isMap(svc)) throw new Error(`Service "${input.service}" was not found in the spec.`);

  const skipped: string[] = [];
  const secretValues: Record<string, string> = {};
  svc.delete('env_file');

  const envNode: unknown = svc.get('environment', true);
  const plainToAdd = input.plain.filter((p) => {
    if (envHas(envNode, p.key)) {
      skipped.push(p.key);
      return false;
    }
    return true;
  });
  const secretsList: unknown = svc.get('secrets', true);
  const secretsToAdd = input.secrets.filter((s) => {
    if (envHas(envNode, s.key) || secretEnvHas(secretsList, s.key)) {
      skipped.push(s.key);
      return false;
    }
    return true;
  });

  if (plainToAdd.length) {
    let env: YAMLMap | YAMLSeq;
    if (isMap(envNode) || isSeq(envNode)) env = envNode;
    else {
      env = new YAMLMap();
      svc.set('environment', env);
    }
    for (const p of plainToAdd) {
      if (isMap(env)) env.set(p.key, p.value);
      else env.add(`${p.key}=${p.value}`);
    }
  }

  if (secretsToAdd.length) {
    const topNode: unknown = doc.get('secrets', true);
    let top: YAMLMap;
    if (isMap(topNode)) top = topNode;
    else {
      top = new YAMLMap();
      doc.set('secrets', top);
    }
    let list: YAMLSeq;
    if (isSeq(secretsList)) list = secretsList;
    else {
      list = new YAMLSeq();
      svc.set('secrets', list);
    }
    for (const s of secretsToAdd) {
      const decl = top.get(s.name, true);
      if (!isMap(decl)) top.set(s.name, doc.createNode({ file: `./secrets/${s.name}` }));
      else if (!decl.has('file')) decl.set('file', `./secrets/${s.name}`);
      list.add(doc.createNode({ source: s.name, env: s.key }));
      secretValues[s.name] = s.value;
    }
  }

  return { spec: doc.toString({ lineWidth: 0 }), skipped, secretValues };
}

/** Names of top-level secrets the spec declares. */
export function declaredSecrets(spec: string): string[] {
  if (!spec.includes('secrets')) return [];
  const doc = parseDocument(spec);
  const top = doc.get('secrets', true);
  if (!isMap(top)) return [];
  return top.items.map((p) => String(isScalar(p.key) ? p.key.value : p.key));
}
