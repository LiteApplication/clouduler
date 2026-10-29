// Serves /stacks/new and /stacks/:name/deploy: one editor, live validation,
// and a placement preview, so a bad spec is caught before the Deploy button.

import { useEffect, useMemo, useRef, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { api, ApiError, decodeText, encodeText } from '../../api/client';
import type { StackValidationResult } from '../../api/types';
import { Banner, Button, EmptyState, Icon, KV, PageHeader, Panel, Tag, TextField, useToast } from '../../ds';
import { StackSpecEditor } from '../../components/StackSpecEditor';
import { diffLines, lineForPath } from '../../lib/specText';
import { useClusterState } from '../../state/store';
import { useAuth } from '../../auth/AuthContext';
import { PlacementPanel } from './PlacementPanel';
import { DiffView } from './StackDetail';
import { allConfigKeys, allSecretKeys, useKnownNames } from './specTools';
import './workloads.css';

const DEBOUNCE_MS = 400;

export function Deploy() {
  const { name: routeName } = useParams();
  const isNew = !routeName;
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const navigate = useNavigate();
  const toast = useToast();
  const known = useKnownNames();
  const existing = routeName ? cluster?.stacks[routeName] : undefined;

  const [nameInput, setNameInput] = useState(routeName ?? '');
  const [spec, setSpec] = useState('');
  const seeded = useRef(false);
  const [result, setResult] = useState<StackValidationResult | null>(null);
  const [checking, setChecking] = useState(false);
  const [checkError, setCheckError] = useState<string | null>(null);
  const [configDrafts, setConfigDrafts] = useState<Record<string, string>>({});
  const [secretDrafts, setSecretDrafts] = useState<Record<string, string>>({});
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Redeploys start from the running spec, once the cluster has loaded.
  useEffect(() => {
    if (seeded.current || !existing) return;
    seeded.current = true;
    setSpec(existing.spec);
  }, [existing]);

  // Live validation, debounced so typing doesn't hammer the api.
  useEffect(() => {
    if (!spec.trim()) {
      setResult(null);
      setChecking(false);
      return;
    }
    let cancelled = false;
    setChecking(true);
    const t = window.setTimeout(() => {
      api
        .validateStack(nameInput.trim() || 'preview', spec)
        .then((r) => {
          if (cancelled) return;
          setResult(r);
          setCheckError(null);
        })
        .catch((e) => {
          if (!cancelled) setCheckError(e instanceof ApiError ? e.message : 'The spec could not be checked. Try again.');
        })
        .finally(() => !cancelled && setChecking(false));
    }, DEBOUNCE_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(t);
    };
  }, [spec, nameInput]);

  const requirements = result?.requirements ?? null;

  // A draft field for every config/secret the spec references, seeded once so
  // anything already typed is not clobbered by the next validation.
  useEffect(() => {
    if (!requirements) return;
    setConfigDrafts((prev) => {
      const next = { ...prev };
      for (const key of allConfigKeys(requirements)) {
        if (!(key in next)) {
          const have = cluster?.configs[key];
          next[key] = have ? decodeText(have.content) : '';
        }
      }
      return next;
    });
    setSecretDrafts((prev) => {
      const next = { ...prev };
      for (const key of allSecretKeys(requirements)) if (!(key in next)) next[key] = '';
      return next;
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [requirements]);

  const errors = result?.errors ?? [];
  const errorLines = useMemo(
    () => errors.map((e) => lineForPath(spec, e.path)).filter((n): n is number => n != null),
    [errors, spec],
  );
  const changes = useMemo(
    () => (existing && spec && existing.spec !== spec ? diffLines(existing.spec, spec) : []),
    [existing, spec],
  );

  if (!isAdmin) {
    return (
      <EmptyState icon="lock" title="Deploying needs an admin">
        Ask an admin to deploy this stack, or sign in with an admin API key.
      </EmptyState>
    );
  }
  if (routeName && cluster && !existing) {
    return (
      <EmptyState icon="stack" title="No stack called that" action={<Button variant="primary" onClick={() => navigate('/stacks/new')}>Deploy a new stack</Button>}>
        {`There is no stack named ${routeName} to redeploy.`}
      </EmptyState>
    );
  }

  const trimmed = nameInput.trim();
  const valid = !!result?.valid && !checking && !checkError;
  const canDeploy = valid && !!trimmed && !busy;

  const deploy = async () => {
    setBusy(true);
    setSubmitError(null);
    let step = 'validating spec';
    try {
      const r = await api.validateStack(trimmed, spec);
      setResult(r);
      if (!r.valid || !r.requirements) return;

      step = 'encoding config content';
      const configs: Record<string, string> = {};
      for (const key of r.requirements.file_configs) {
        const draft = configDrafts[key];
        if (draft) configs[key] = encodeText(draft);
        else if (!cluster?.configs[key]) {
          setSubmitError(`Config "${key}" needs content before this stack can deploy. Add it under Bundled files.`);
          return;
        }
      }
      step = 'encoding secret content';
      const secrets: Record<string, string> = {};
      for (const key of r.requirements.file_secrets) {
        const draft = secretDrafts[key];
        if (draft) secrets[key] = encodeText(draft);
        else if (!cluster?.secrets[key]) {
          setSubmitError(`Secret "${key}" needs a value before this stack can deploy. Add it under Bundled files.`);
          return;
        }
      }

      // Configs/secrets the spec only refers to by name aren't bundled with the
      // deploy request, so any edited content for them is saved on its own.
      step = 'saving standalone config content';
      for (const key of r.requirements.configs) {
        if (r.requirements.file_configs.includes(key)) continue;
        const draft = configDrafts[key];
        if (draft) await api.putConfig(key, encodeText(draft));
      }
      step = 'saving standalone secret content';
      for (const key of r.requirements.secrets) {
        if (r.requirements.file_secrets.includes(key)) continue;
        const draft = secretDrafts[key];
        if (draft) await api.putSecret(key, { content: encodeText(draft) });
      }

      step = 'submitting deploy';
      await api.deployStack(trimmed, { spec, configs, secrets });
      toast(`Deployed ${trimmed}`);
      navigate(`/stacks/${encodeURIComponent(trimmed)}`);
    } catch (e) {
      const message = e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e);
      console.error(`[deploy] ${trimmed || '(unnamed)'} failed while ${step}:`, e);
      setSubmitError(`The deploy failed while ${step}: ${message}`);
    } finally {
      setBusy(false);
    }
  };

  const configKeys = requirements ? allConfigKeys(requirements) : [];
  const secretKeys = requirements ? allSecretKeys(requirements) : [];
  const need = (items: string[]) => (items.length ? items.map((x) => <Tag key={x}>{x}</Tag>) : 'None');

  return (
    <div className="sec wl-page">
      <PageHeader
        breadcrumb={[{ label: 'Stacks', to: '/stacks' }, ...(routeName ? [{ label: routeName, to: `/stacks/${encodeURIComponent(routeName)}` }] : []), { label: 'Deploy' }]}
        title={isNew ? 'Deploy stack' : `Redeploy ${routeName}`}
        sub={isNew ? 'Paste or write a stack spec. It is checked as you type.' : `Edit the spec of ${routeName} and deploy a new revision.`}
        actions={
          <>
            <Button variant="ghost" onClick={() => navigate(routeName ? `/stacks/${encodeURIComponent(routeName)}` : '/stacks')} disabled={busy}>
              Cancel
            </Button>
            <Button variant="primary" icon="upload" disabled={!canDeploy} loading={busy} onClick={deploy}>
              {`Deploy ${trimmed || 'stack'}`}
            </Button>
          </>
        }
      />

      <div className="split">
        <div className="split__main">
          <TextField label="Stack name" mono value={nameInput} disabled={!isNew} onChange={(e) => setNameInput(e.target.value)} hint={isNew ? 'Lowercase letters, numbers and dashes.' : undefined} />
          <div className="sec">
            <div className="sec__h">
              <h2 className="h2">Spec</h2>
              <label className="wl-file">
                <Icon name="file" size={16} /> Load from file
                <input type="file" accept=".yml,.yaml" onChange={(e) => e.target.files?.[0]?.text().then(setSpec)} />
              </label>
            </div>
            <StackSpecEditor value={spec} onChange={setSpec} editable known={known} minHeight="360px" errorLines={errorLines} />
          </div>
          <Validation checking={checking} spec={spec} result={result} checkError={checkError} />
          {changes.length > 0 && (
            <Panel title="Changes to deploy">
              <DiffView lines={changes} />
            </Panel>
          )}
        </div>

        <aside className="split__side">
          <Panel title="What it needs">
            {requirements ? (
              <KV
                items={[
                  ['Networks', need(requirements.networks)],
                  ['Drives', need(requirements.drives)],
                  ['Configs', need(configKeys)],
                  ['Secrets', need(secretKeys)],
                  ['Hosts', need(requirements.hosts)],
                ]}
              />
            ) : (
              <p className="sub">Add a valid spec to see what it needs from the cluster.</p>
            )}
          </Panel>
        </aside>
      </div>

      {(configKeys.length > 0 || secretKeys.length > 0) && requirements && (
        <Panel title="Bundled files">
          <p className="sub">
            Every config and secret this spec references, whether bundled with a <code>file:</code> or already stored in the cluster. Leave a field blank to keep what is stored.
          </p>
          {configKeys.map((key) => (
            <div className="sec" key={`config-${key}`}>
              <div className="row">
                <strong className="mono">{`config: ${key}`}</strong>
                {!cluster?.configs[key] && <Tag tone="warn">New</Tag>}
                {!requirements.file_configs.includes(key) && <Tag>Standalone</Tag>}
              </div>
              <input type="file" aria-label={`Load ${key} from a file`} onChange={(e) => e.target.files?.[0]?.text().then((text) => setConfigDrafts((p) => ({ ...p, [key]: text })))} />
              <TextField
                label={`Content of ${key}`}
                multiline
                mono
                rows={6}
                value={configDrafts[key] ?? ''}
                placeholder={cluster?.configs[key] ? 'Leave blank to keep the current content' : 'File content'}
                onChange={(e) => setConfigDrafts((p) => ({ ...p, [key]: e.target.value }))}
              />
            </div>
          ))}
          {secretKeys.map((key) => (
            <div className="sec" key={`secret-${key}`}>
              <div className="row">
                <strong className="mono">{`secret: ${key}`}</strong>
                {!cluster?.secrets[key] && <Tag tone="warn">New</Tag>}
                {!requirements.file_secrets.includes(key) && <Tag>Standalone</Tag>}
              </div>
              <input type="file" aria-label={`Load ${key} from a file`} onChange={(e) => e.target.files?.[0]?.text().then((text) => setSecretDrafts((p) => ({ ...p, [key]: text })))} />
              <TextField
                label={`Value of ${key}`}
                multiline
                mono
                rows={4}
                value={secretDrafts[key] ?? ''}
                placeholder={cluster?.secrets[key] ? 'Leave blank to keep the current value' : 'Secret value'}
                onChange={(e) => setSecretDrafts((p) => ({ ...p, [key]: e.target.value }))}
              />
            </div>
          ))}
        </Panel>
      )}

      {result && Object.keys(result.per_service).length > 0 && (
        <Panel title="Where it can run">
          <PlacementPanel perService={result.per_service} />
        </Panel>
      )}

      {submitError && (
        <Banner tone="danger" title="Not deployed">
          {submitError}
        </Banner>
      )}
    </div>
  );
}

function Validation({
  checking,
  spec,
  result,
  checkError,
}: {
  checking: boolean;
  spec: string;
  result: StackValidationResult | null;
  checkError: string | null;
}) {
  return (
    <div className="wl-validation" aria-live="polite">
      {checkError ? (
        <Banner tone="danger" title="Could not check the spec">
          {checkError}
        </Banner>
      ) : !spec.trim() ? (
        <p className="sub">Nothing to check yet.</p>
      ) : checking && !result ? (
        <p className="sub">Checking the spec...</p>
      ) : result && result.errors.length > 0 ? (
        <Banner tone="danger" title={`${result.errors.length} ${result.errors.length === 1 ? 'problem' : 'problems'} to fix before you can deploy`}>
          <ul className="wl-errors">
            {result.errors.map((err, i) => (
              <li key={i}>
                {err.path && <span className="mono">{err.path}: </span>}
                {err.message}
              </li>
            ))}
          </ul>
        </Banner>
      ) : result ? (
        <Banner tone="ok" title="Spec is valid" />
      ) : null}
    </div>
  );
}
