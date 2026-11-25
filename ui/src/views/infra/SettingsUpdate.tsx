// Self-update. SettingsUpdate (Settings > Updates) is where releases come from
// and how they are checked; SelfUpdatePanel (the Updates page) is what is
// running against what is available, the per-node phases, and Start / Cancel.

import { useEffect, useRef, useState } from 'react';
import { api, ApiError } from '../../api/client';
import type { SelfUpdateCheck, SelfUpdateConfig, SelfUpdateGithubTokenStatus, SelfUpdateLatest, SelfUpdateNodePhase, SelfUpdateRunState } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Banner, Button, KV, Panel, Tag, TextField, useConfirm, useToast, type TagTone } from '../../ds';
import { useClusterState } from '../../state/store';
import { bare, isNewer, planUpdatePath } from '../../components/version';
import { RollbackRelease } from './RollbackRelease';

const RUN_CONFIG = '_clouduler_self_update_run';
const CHECK_CONFIG = '_clouduler_self_update_check';
const DEFAULT_RELEASE_URL = 'https://github.com/LiteApplication/clouduler';
const STEP_POLL_MS = 3000;
const STEP_TIMEOUT_MS = 30 * 60 * 1000;

/** The run state rides `ClusterState.configs` as base64 json. */
function decodeRun(content: string | undefined): SelfUpdateRunState | null {
  if (!content) return null;
  try {
    return JSON.parse(atob(content)) as SelfUpdateRunState;
  } catch {
    return null;
  }
}

function decodeCheck(content: string | undefined): SelfUpdateCheck | null {
  if (!content) return null;
  try {
    return JSON.parse(atob(content)) as SelfUpdateCheck;
  } catch {
    return null;
  }
}

function phaseLabel(phase: SelfUpdateNodePhase): string {
  if (typeof phase === 'string') return phase.replace(/_/g, ' ');
  const [key, value] = Object.entries(phase)[0];
  return `${key.replace(/_/g, ' ')}: ${(value as { reason: string }).reason}`;
}

function phaseTone(phase: SelfUpdateNodePhase): TagTone | undefined {
  if (typeof phase !== 'string') return 'danger';
  if (phase === 'done') return 'ok';
  return phase === 'idle' ? undefined : 'warn';
}

export function SettingsUpdate() {
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const toast = useToast();
  const [githubToken, setGithubToken] = useState<SelfUpdateGithubTokenStatus | null>(null);
  const [repo, setRepo] = useState<SelfUpdateConfig | null>(null);
  const [releaseUrl, setReleaseUrl] = useState(DEFAULT_RELEASE_URL);
  const [tokenInput, setTokenInput] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.selfUpdateGithubToken().then(setGithubToken).catch(() => {});
    api.selfUpdateRepo().then(setRepo).catch(() => {});
  }, []);

  const check = decodeCheck(cluster?.configs[CHECK_CONFIG]?.content);
  const checking = repo?.check_updates ?? true;
  const [interval, setInterval_] = useState('');

  const attempt = async (fn: () => Promise<unknown>, fallback: string) => {
    setError(null);
    setBusy(true);
    try {
      await fn();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : fallback);
    } finally {
      setBusy(false);
    }
  };

  if (!cluster) return <p className="cl-muted">Loading…</p>;

  return (
    <div className="stack">
      {error && <Banner tone="danger" onClose={() => setError(null)}>{error}</Banner>}

      <Panel title="Release source" actions={githubToken?.configured && <Tag tone="ok">GitHub token set</Tag>}>
        <p className="cl-sm cl-muted">
          Point Clouduler at a GitHub repo to track its latest release, or at one release to pin it. Each node downloads
          its own binary directly.
        </p>
        <TextField
          label="Repository"
          mono
          placeholder={DEFAULT_RELEASE_URL}
          value={repo?.release_url ?? releaseUrl}
          onChange={(e) => setReleaseUrl(e.target.value)}
          disabled={!!repo || !isAdmin}
        />
        {isAdmin && !repo && (
          <Button variant="secondary" disabled={busy || !releaseUrl.trim()} onClick={() => void attempt(async () => setRepo((await api.setSelfUpdateRepo({ release_url: releaseUrl.trim() }), await api.selfUpdateRepo())), 'Could not save the release.')}>
            Save release
          </Button>
        )}
        {isAdmin && (
          <>
            <TextField
              label="GitHub token"
              type="password"
              placeholder={githubToken?.configured ? 'Already set' : 'Needed for a private repo or higher rate limits'}
              value={tokenInput}
              onChange={(e) => setTokenInput(e.target.value)}
            />
            <Button
              disabled={busy || !tokenInput.trim()}
              onClick={() =>
                void attempt(async () => {
                  await api.setSelfUpdateGithubToken(tokenInput.trim());
                  setTokenInput('');
                  setGithubToken(await api.selfUpdateGithubToken());
                  toast('Saved the GitHub token.');
                }, 'Could not save the token.')
              }
            >
              {githubToken?.configured ? 'Replace token' : 'Save token'}
            </Button>
          </>
        )}
      </Panel>

      {repo && (
        <Panel title="Automatic checks" actions={<Tag tone={checking ? 'ok' : undefined}>{checking ? 'on' : 'off'}</Tag>}>
          <p className="cl-sm cl-muted">
            The leader looks up the latest release every {repo.check_interval ?? '24h'} and records it, so a new
            version shows up here without anyone asking. Nothing is installed automatically.
          </p>
          {check && (
            <p className="cl-sm cl-muted">
              Last checked {new Date(check.checked_at * 1000).toLocaleString()}
              {check.tag ? <>: {check.tag}</> : <>, failed: {check.error}</>}
            </p>
          )}
          {isAdmin && (
            <>
              <TextField label="Check every" mono placeholder={repo.check_interval ?? '24h'} value={interval} onChange={(e) => setInterval_(e.target.value)} />
              <Button
                variant="secondary"
                disabled={busy || !interval.trim()}
                onClick={() =>
                  void attempt(async () => {
                    await api.setSelfUpdateCheck({ enabled: checking, interval: interval.trim() });
                    setRepo(await api.selfUpdateRepo());
                    setInterval_('');
                    toast('Saved the check interval.');
                  }, 'Could not save the interval.')
                }
              >
                Save interval
              </Button>
              <Button
                variant="secondary"
                disabled={busy}
                onClick={() =>
                  void attempt(async () => {
                    await api.setSelfUpdateCheck({ enabled: !checking });
                    setRepo(await api.selfUpdateRepo());
                  }, 'Could not change the setting.')
                }
              >
                {checking ? 'Turn off' : 'Turn on'}
              </Button>
            </>
          )}
        </Panel>
      )}

      {repo && isAdmin && <RollbackRelease />}
    </div>
  );
}

/** The two ways to update when releases must be crossed one series at a time. */
function UpdateModeChoice({ steps, onChange }: { steps: string[]; onChange: (m: 'all' | 'first') => void }) {
  const [mode, setMode] = useState<'all' | 'first'>('all');
  const pick = (m: 'all' | 'first') => { setMode(m); onChange(m); };
  return (
    <div className="stack">
      <p className="cl-sm cl-muted">
        Updates go through the last patch of each minor series first. Nodes download each release, then restart one at a time.
      </p>
      <label className="li">
        <input type="radio" name="update-mode" checked={mode === 'all'} onChange={() => pick('all')} />
        <span><strong>Update step by step</strong> to {steps[steps.length - 1]}<br /><span className="cl-sm cl-muted">{steps.join(' → ')}, one after the other. Keep this page open: the steps are driven from your browser.</span></span>
      </label>
      <label className="li">
        <input type="radio" name="update-mode" checked={mode === 'first'} onChange={() => pick('first')} />
        <span><strong>Update to {steps[0]} only</strong><br /><span className="cl-sm cl-muted">Install this one step now and come back for the rest later.</span></span>
      </label>
    </div>
  );
}

/** The cluster's own release: what runs, what is available, per-node phases, Start / Cancel. */
export function SelfUpdatePanel() {
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const confirm = useConfirm();
  const [repo, setRepo] = useState<SelfUpdateConfig | null>(null);
  const [latest, setLatest] = useState<SelfUpdateLatest | null>(null);
  const [releases, setReleases] = useState<string[] | null>(null);
  // The releases to install in order: the last patch of every minor series first.
  const path = latest && releases ? planUpdatePath(latest.running_version, releases) : null;
  const newer = path ? path.length > 0 : !!latest && isNewer(latest.tag, latest.running_version);
  const upToDate = !!latest && !newer;
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [stepping, setStepping] = useState<{ index: number; total: number; tag: string } | null>(null);
  const stopRequested = useRef(false);
  const unmounted = useRef(false);

  const loadLatest = () => api.selfUpdateLatest().then(setLatest).catch(() => {});
  useEffect(() => {
    unmounted.current = false;
    api.selfUpdateRepo().then(setRepo).catch(() => {});
    void loadLatest();
    api.selfUpdateReleases().then((r) => setReleases(r.tags)).catch(() => {});
    return () => {
      unmounted.current = true;
    };
  }, []);

  const run = decodeRun(cluster?.configs[RUN_CONFIG]?.content);
  const inProgress = !!run && !run.finished_at;

  const attempt = async (fn: () => Promise<unknown>, fallback: string) => {
    setError(null);
    setBusy(true);
    try {
      await fn();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : fallback);
    } finally {
      setBusy(false);
    }
  };

  const toast = useToast();

  /** Wait for the run started for `tag` to finish and the cluster to report that version.
   *  Network errors and 503s are expected while the leader restarts. */
  const waitForStep = async (tag: string, runId: string): Promise<string | null> => {
    const deadline = Date.now() + STEP_TIMEOUT_MS;
    while (Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, STEP_POLL_MS));
      if (unmounted.current) return 'stopped';
      let run: SelfUpdateRunState | null;
      try {
        run = await api.selfUpdateRun();
      } catch {
        continue;
      }
      if (!run || run.id !== runId) continue;
      if (run.failure_reason) return run.failure_reason;
      if (!run.finished_at) continue;
      try {
        const l = await api.selfUpdateLatest();
        if (bare(l.running_version) === bare(tag)) {
          setLatest(l);
          return null;
        }
      } catch {
        // the node answering is restarting
      }
    }
    return `Timed out waiting for ${tag}.`;
  };

  const runSteps = async (steps: string[]) => {
    stopRequested.current = false;
    setError(null);
    try {
      for (let i = 0; i < steps.length; i++) {
        const tag = steps[i];
        if (unmounted.current) return;
        setStepping({ index: i + 1, total: steps.length, tag });
        let started: SelfUpdateRunState;
        try {
          started = await api.startSelfUpdate(tag);
        } catch (e) {
          setError(e instanceof ApiError ? e.message : `Could not start the update to ${tag}.`);
          return;
        }
        const failure = await waitForStep(tag, started.id);
        if (unmounted.current) return;
        if (failure) {
          setError(`Step ${i + 1} of ${steps.length} (${tag}) failed: ${failure}`);
          return;
        }
        if (stopRequested.current && i < steps.length - 1) {
          toast(`Stopped after ${tag}.`);
          return;
        }
      }
      toast('Finished the step-by-step update.');
    } finally {
      if (!unmounted.current) {
        setStepping(null);
        void loadLatest();
      }
    }
  };

  const start = async () => {
    const tag = path?.[0] ?? latest?.tag;
    const label = tag ?? 'the latest release';
    const multi = !!path && path.length > 1;
    const mode = { current: 'all' as 'all' | 'first' };
    const ok = await confirm({
      title: multi ? 'Update Clouduler' : `Update to ${label}?`,
      body: multi
        ? <UpdateModeChoice steps={path} onChange={(m) => { mode.current = m; }} />
        : 'Nodes download the release, then restart one at a time once all are ready.',
      confirmLabel: multi ? 'Update' : `Update to ${label}`,
      confirmVariant: 'primary',
    });
    if (!ok) return;
    if (multi && mode.current === 'all') await runSteps(path);
    else await attempt(() => api.startSelfUpdate(path?.[0]), 'Could not start the update.');
  };

  const cancel = async () => {
    if (!(await confirm({ title: 'Cancel the update?', body: 'Nodes that already restarted stay on the new version.', confirmLabel: 'Cancel update', confirmVariant: 'danger' }))) return;
    await attempt(() => api.cancelSelfUpdate(), 'Could not cancel the update.');
  };

  if (!cluster) return null;
  const nodes = Object.values(cluster.nodes);

  return (
    <>
      {error && <Banner tone="danger" onClose={() => setError(null)}>{error}</Banner>}
      {run?.failure_reason && <Banner tone="warn" title="The update stopped">{run.failure_reason}</Banner>}
      <Panel
        title="Clouduler"
        actions={isAdmin && (stepping
          ? (
            <>
              <Button variant="secondary" disabled={stopRequested.current} onClick={() => { stopRequested.current = true; setStepping({ ...stepping }); }}>Stop after this step</Button>
              {inProgress && <Button variant="danger" icon="x" onClick={() => void cancel()}>Cancel update</Button>}
            </>
          )
          : inProgress
            ? <Button variant="danger" icon="x" onClick={() => void cancel()}>Cancel update</Button>
            : (
              <>
                <Button icon="play" disabled={busy || !repo || upToDate} onClick={() => void start()}>Start update{newer && !(path && path.length > 1) ? ` to ${path?.[0] ?? latest?.tag}` : ''}</Button>
              </>
            ))}
      >
        <KV items={[
          ['Running', latest?.running_version ?? '-'],
          ['Available', latest ? <>{latest.tag}{newer && <> <Tag tone="brand">newer</Tag></>}</> : '-'],
          ...(run?.release_tag ? [['Rolling out', run.release_tag] as [string, string]] : []),
        ]} />
        {!repo && <p className="cl-sm cl-muted">Set a release source before starting an update.</p>}
        {path && path.length > 1 && !stepping && (
          <p className="cl-sm cl-muted">
            Updates go through the last patch of each minor series first: {path.join(' → ')}. Start installs {path[0]}.
          </p>
        )}
        {stepping && (
          <p className="cl-sm">
            Step {stepping.index} of {stepping.total}: {stepping.tag}
            {stopRequested.current && <span className="cl-muted"> (stopping after this step)</span>}
          </p>
        )}
        {upToDate && !inProgress && <p className="cl-sm cl-muted">Already on the latest version.</p>}
        {run ? (
          <div className="stack">
            {nodes.map((n) => {
              const phase = run.per_node[n.name];
              return (
                <div className="li" key={n.name}>
                  <strong>{n.name}</strong>
                  <span className="cl-sm cl-muted">{n.arch || 'unknown'}</span>
                  <span style={{ flex: 1 }} />
                  {phase ? <Tag tone={phaseTone(phase)}>{phaseLabel(phase)}</Tag> : <span className="cl-muted">-</span>}
                </div>
              );
            })}
          </div>
        ) : (
          <p className="cl-sm cl-muted">No update has been started yet.</p>
        )}
      </Panel>
    </>
  );
}
