// Imports a service's `env_file` in the deploy editor. The server never sees
// the file, so the user pastes its text here, picks plain or secret per
// variable, and the spec is rewritten to carry the entries itself. Pasted
// values live only in this component's state until applied.

import { useMemo, useState } from 'react';
import { Banner, Button, CheckOption, Icon, Panel, TextField, useToast } from '../../ds';
import { parseDotenv } from '../../lib/dotenv';
import {
  applyEnvImport,
  declaredSecrets,
  defaultSecretName,
  detectEnvFiles,
  looksSensitive,
  secretNameError,
  type EnvFileRef,
} from '../../lib/envImport';

interface Choice {
  secret: boolean;
  name: string;
  overwrite: boolean;
}

export function EnvImportPanel({
  spec,
  clusterSecrets,
  onApply,
}: {
  spec: string;
  /** Names of secrets already stored in the cluster. */
  clusterSecrets: string[];
  /** Receives the rewritten spec and the content to bundle per secret name. */
  onApply: (spec: string, secretValues: Record<string, string>) => void;
}) {
  const refs = useMemo(() => detectEnvFiles(spec), [spec]);
  if (refs.length === 0) return null;
  return (
    <>
      {refs.map((ref) => (
        <EnvImportService key={ref.service} spec={spec} ref_={ref} clusterSecrets={clusterSecrets} onApply={onApply} />
      ))}
    </>
  );
}

function EnvImportService({
  spec,
  ref_,
  clusterSecrets,
  onApply,
}: {
  spec: string;
  ref_: EnvFileRef;
  clusterSecrets: string[];
  onApply: (spec: string, secretValues: Record<string, string>) => void;
}) {
  const { service, files } = ref_;
  const toast = useToast();
  const [text, setText] = useState('');
  const [reviewing, setReviewing] = useState(false);
  const [choices, setChoices] = useState<Record<string, Choice>>({});
  const [reveal, setReveal] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const parsed = useMemo(() => parseDotenv(text), [text]);
  const inSpec = useMemo(() => new Set(declaredSecrets(spec)), [spec]);
  const inCluster = useMemo(() => new Set(clusterSecrets), [clusterSecrets]);

  const startReview = () => {
    const next: Record<string, Choice> = {};
    for (const e of parsed.entries) {
      next[e.key] = { secret: e.value !== '' && looksSensitive(e.key), name: defaultSecretName(service, e.key), overwrite: false };
    }
    setChoices(next);
    setReveal(false);
    setError(null);
    setReviewing(true);
  };

  const set = (key: string, patch: Partial<Choice>) => setChoices((p) => ({ ...p, [key]: { ...p[key], ...patch } }));

  // Per-key problems that block applying.
  const secretEntries = parsed.entries.filter((e) => choices[e.key]?.secret);
  const problems: Record<string, string> = {};
  const seen = new Map<string, string>();
  for (const e of secretEntries) {
    const c = choices[e.key];
    const bad = secretNameError(c.name);
    if (bad) problems[e.key] = bad;
    else if (seen.has(c.name)) problems[e.key] = `Same name as ${seen.get(c.name)}.`;
    else if ((inSpec.has(c.name) || inCluster.has(c.name)) && !c.overwrite) problems[e.key] = 'Confirm the overwrite or pick another name.';
    seen.set(c.name, e.key);
  }
  const canApply = reviewing && parsed.entries.length > 0 && Object.keys(problems).length === 0;

  const apply = () => {
    try {
      const out = applyEnvImport(spec, {
        service,
        plain: parsed.entries.filter((e) => !choices[e.key]?.secret).map((e) => ({ key: e.key, value: e.value })),
        secrets: secretEntries.map((e) => ({ key: e.key, name: choices[e.key].name, value: e.value })),
      });
      onApply(out.spec, out.secretValues);
      // The pasted text and values are dropped with this component's state.
      setText('');
      setChoices({});
      setReviewing(false);
      if (out.skipped.length) {
        toast(`Skipped ${out.skipped.join(', ')}: ${service} already sets ${out.skipped.length === 1 ? 'it' : 'them'} in the spec.`);
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : 'The spec could not be rewritten.');
    }
  };

  return (
    <Panel title={`Environment file for ${service}`}>
      <p className="sub">
        Service <code>{service}</code> sets <code>env_file</code>
        {files.length > 0 && (
          <>
            {': '}
            {files.map((f, i) => (
              <span key={f}>
                {i > 0 && ', '}
                <code>{f}</code>
              </span>
            ))}
          </>
        )}
        . A stack cannot read it, so paste its contents here to move the entries into the spec.
        {files.length > 1 && ' Paste the files one after the other, in the order listed. A later value replaces an earlier one.'}
      </p>
      {!reviewing ? (
        <>
          <TextField
            label={`Contents of ${files.length > 1 ? 'the env files' : files[0] ?? 'the env file'}`}
            multiline
            mono
            rows={8}
            value={text}
            placeholder={'KEY=value\nexport OTHER="quoted value"\n# comments are ignored'}
            onChange={(e) => setText(e.target.value)}
          />
          <ParseIssues parsed={parsed} />
          <div className="row">
            <Button variant="primary" disabled={parsed.entries.length === 0} onClick={startReview}>
              {`Review ${parsed.entries.length} ${parsed.entries.length === 1 ? 'variable' : 'variables'}`}
            </Button>
          </div>
        </>
      ) : (
        <>
          <ParseIssues parsed={parsed} />
          <div className="row">
            <Button size="sm" icon="eye" onClick={() => setReveal((r) => !r)}>
              {reveal ? 'Hide values' : 'Show values'}
            </Button>
          </div>
          {parsed.entries.map((e) => {
            const c = choices[e.key];
            if (!c) return null;
            const conflict = c.secret && (inSpec.has(c.name) || inCluster.has(c.name));
            return (
              <div className="sec" key={e.key}>
                <div className="row">
                  <strong className="mono">{e.key}</strong>
                  <span className="mono">{reveal ? e.value || '(empty)' : e.value ? '•'.repeat(Math.min(e.value.length, 12)) : '(empty)'}</span>
                </div>
                <div className="row" role="group" aria-label={`How to store ${e.key}`}>
                  <Button size="sm" variant={c.secret ? 'secondary' : 'primary'} onClick={() => set(e.key, { secret: false })}>
                    Plain
                  </Button>
                  <Button size="sm" variant={c.secret ? 'primary' : 'secondary'} disabled={e.value === ''} onClick={() => set(e.key, { secret: true })}>
                    Secret
                  </Button>
                  {e.value === '' && <span className="sub">An empty value cannot be a secret.</span>}
                </div>
                {c.secret && (
                  <>
                    <TextField
                      label={`Secret name for ${e.key}`}
                      mono
                      value={c.name}
                      error={problems[e.key] && problems[e.key] !== 'Confirm the overwrite or pick another name.' ? problems[e.key] : undefined}
                      onChange={(ev) => set(e.key, { name: ev.target.value, overwrite: false })}
                    />
                    {conflict && (
                      <CheckOption checked={c.overwrite} onChange={(on) => set(e.key, { overwrite: on })}>
                        {inCluster.has(c.name)
                          ? `A cluster secret called ${c.name} already exists. Overwrite it on deploy. Every stack that uses it gets the new value, and a running service may break.`
                          : `${c.name} is already declared in this spec. Replace its content with this value.`}
                      </CheckOption>
                    )}
                  </>
                )}
              </div>
            );
          })}
          {error && <Banner tone="danger" title="Could not import">{error}</Banner>}
          <div className="row">
            <Button variant="ghost" onClick={() => setReviewing(false)}>
              <Icon name="edit" size={16} /> Edit the text
            </Button>
            <Button
              variant="ghost"
              onClick={() => {
                setText('');
                setReviewing(false);
              }}
            >
              Discard
            </Button>
            <Button variant="primary" icon="check" disabled={!canApply} onClick={apply}>
              Apply to the spec
            </Button>
          </div>
          <p className="sub">
            Plain variables go under <code>environment</code>. Secrets are added as bundled <code>file: ./secrets/NAME</code> entries exposed with <code>env:</code>, and their values appear under Bundled files. A variable the service already sets in <code>environment</code> keeps its value in the spec.
          </p>
        </>
      )}
    </Panel>
  );
}

function ParseIssues({ parsed }: { parsed: ReturnType<typeof parseDotenv> }) {
  if (!parsed.errors.length && !parsed.warnings.length) return null;
  return (
    <>
      {parsed.errors.length > 0 && (
        <Banner tone="danger" title={`${parsed.errors.length} ${parsed.errors.length === 1 ? 'line' : 'lines'} could not be read and will not be imported`}>
          <ul className="wl-errors">
            {parsed.errors.map((e, i) => (
              <li key={i}>{`Line ${e.line}: ${e.message}`}</li>
            ))}
          </ul>
        </Banner>
      )}
      {parsed.warnings.length > 0 && (
        <Banner tone="warn" title="Duplicate keys">
          <ul className="wl-errors">
            {parsed.warnings.map((e, i) => (
              <li key={i}>{`Line ${e.line}: ${e.message}`}</li>
            ))}
          </ul>
        </Banner>
      )}
    </>
  );
}
