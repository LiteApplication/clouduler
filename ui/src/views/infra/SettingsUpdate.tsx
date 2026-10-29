// Settings > Self-update: where releases come from, what is running against
// what is available, the per-node phases, and Start / Cancel.

import { useEffect, useState } from 'react';
import { api, ApiError } from '../../api/client';
import type { SelfUpdateConfig, SelfUpdateGithubTokenStatus, SelfUpdateLatest, SelfUpdateNodePhase, SelfUpdateRunState } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Banner, Button, KV, Panel, Tag, TextField, useConfirm, useToast, type TagTone } from '../../ds';
import { useClusterState } from '../../state/store';

const RUN_CONFIG = '_clouduler_self_update_run';
const DEFAULT_RELEASE_URL = 'https://github.com/LiteApplication/clouduler';

/** The run state rides `ClusterState.configs` as base64 json. */
function decodeRun(content: string | undefined): SelfUpdateRunState | null {
  if (!content) return null;
  try {
    return JSON.parse(atob(content)) as SelfUpdateRunState;
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

export function SettingsUpdate({ startSignal }: { startSignal: number }) {
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const confirm = useConfirm();
  const toast = useToast();
  const [githubToken, setGithubToken] = useState<SelfUpdateGithubTokenStatus | null>(null);
  const [repo, setRepo] = useState<SelfUpdateConfig | null>(null);
  const [latest, setLatest] = useState<SelfUpdateLatest | null>(null);
  // Tags are `v0.8.0`, the running version is `0.8.0`.
  const newer = !!latest && latest.tag.replace(/^v/, '') !== latest.running_version.replace(/^v/, '');
  const [releaseUrl, setReleaseUrl] = useState(DEFAULT_RELEASE_URL);
  const [tokenInput, setTokenInput] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.selfUpdateGithubToken().then(setGithubToken).catch(() => {});
    api.selfUpdateRepo().then(setRepo).catch(() => {});
    api.selfUpdateLatest().then(setLatest).catch(() => {});
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

  const start = async () => {
    const tag = latest?.tag ?? 'the latest release';
    const ok = await confirm({
      title: `Update to ${tag}?`,
      body: 'Nodes download the release, then restart one at a time once all are ready.',
      confirmLabel: `Update to ${tag}`,
      confirmVariant: 'primary',
    });
    if (ok) await attempt(() => api.startSelfUpdate(), 'Could not start the update.');
  };
  // The header's "Start update" button lives in Settings; it bumps this signal.
  useEffect(() => {
    if (startSignal > 0 && !inProgress && repo) void start();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [startSignal]);

  const cancel = async () => {
    if (!(await confirm({ title: 'Cancel the update?', body: 'Nodes that already restarted stay on the new version.', confirmLabel: 'Cancel update', confirmVariant: 'danger' }))) return;
    await attempt(() => api.cancelSelfUpdate(), 'Could not cancel the update.');
  };

  if (!cluster) return <p className="cl-muted">Loading…</p>;
  const nodes = Object.values(cluster.nodes);

  return (
    <div className="stack">
      {error && <Banner tone="danger" onClose={() => setError(null)}>{error}</Banner>}
      {run?.failure_reason && <Banner tone="warn" title="The update stopped">{run.failure_reason}</Banner>}

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

      <Panel
        title="Update"
        actions={isAdmin && (inProgress
          ? <Button variant="danger" icon="x" onClick={() => void cancel()}>Cancel update</Button>
          : <Button icon="play" disabled={busy || !repo} onClick={() => void start()}>Start update{newer ? ` to ${latest.tag}` : ''}</Button>)}
      >
        <KV items={[
          ['Running', latest?.running_version ?? '-'],
          ['Available', latest ? <>{latest.tag}{newer && <> <Tag tone="brand">newer</Tag></>}</> : '-'],
          ...(run?.release_tag ? [['Rolling out', run.release_tag] as [string, string]] : []),
        ]} />
        {!repo && <p className="cl-sm cl-muted">Set a release source before starting an update.</p>}
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
    </div>
  );
}
