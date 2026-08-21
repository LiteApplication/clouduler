import { useEffect, useState } from 'react';
import { api, ApiError } from '../api/client';
import type { SelfUpdateConfig, SelfUpdateGithubTokenStatus, SelfUpdateNodePhase, SelfUpdateRunState } from '../api/types';
import { useClusterState } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { Badge } from '../components/Badge';
import type { BadgeTone } from '../components/Badge';
import { Icon } from '../components/Icon';

const RUN_CONFIG = '_clouduler_self_update_run';

/** Shown as the release url placeholder and prefilled default: this
 *  project's own repo, pinned to a specific release tag. */
const DEFAULT_RELEASE_URL = 'https://github.com/LiteApplication/clouduler/releases/tag/v0.1.0';

/** Decode the run state out of `ClusterState.configs`, the same object the
 *  api reads it from: the config content is base64 json, so it rides the
 *  existing `/api/events` stream for free without a dedicated poll. */
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

function phaseTone(phase: SelfUpdateNodePhase): BadgeTone {
  if (typeof phase === 'string') {
    if (phase === 'done') return 'good';
    if (phase === 'idle') return 'neutral';
    return 'warn';
  }
  return 'bad';
}

export function SelfUpdate() {
  const cluster = useClusterState();
  const { isAdmin } = useAuth();

  const [githubToken, setGithubTokenStatus] = useState<SelfUpdateGithubTokenStatus | null>(null);
  const [repo, setRepo] = useState<SelfUpdateConfig | null>(null);
  const [releaseUrl, setReleaseUrl] = useState(DEFAULT_RELEASE_URL);
  const [tokenInput, setTokenInput] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.selfUpdateGithubToken().then(setGithubTokenStatus).catch(() => {});
    api.selfUpdateRepo().then(setRepo).catch(() => {});
  }, []);

  const saveToken = async () => {
    if (!tokenInput.trim()) return;
    setError(null);
    setBusy(true);
    try {
      await api.setSelfUpdateGithubToken(tokenInput.trim());
      setTokenInput('');
      setGithubTokenStatus(await api.selfUpdateGithubToken());
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'failed to save the github token');
    } finally {
      setBusy(false);
    }
  };

  const saveRepo = async () => {
    if (!releaseUrl.trim()) return;
    setError(null);
    setBusy(true);
    try {
      await api.setSelfUpdateRepo({ release_url: releaseUrl.trim() });
      setRepo(await api.selfUpdateRepo());
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'failed to save the release url');
    } finally {
      setBusy(false);
    }
  };

  const run = decodeRun(cluster?.configs[RUN_CONFIG]?.content);
  const inProgress = !!run && !run.finished_at;

  const startUpdate = async () => {
    setError(null);
    setBusy(true);
    try {
      await api.startSelfUpdate();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'failed to start the update');
    } finally {
      setBusy(false);
    }
  };

  const cancelUpdate = async () => {
    if (!confirm('Cancel the in-progress update?')) return;
    setError(null);
    try {
      await api.cancelSelfUpdate();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'failed to cancel the update');
    }
  };

  if (!cluster) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading...
      </p>
    );
  }

  const nodesByArch = new Map<string, string[]>();
  for (const node of Object.values(cluster.nodes)) {
    const arch = node.arch || 'unknown';
    nodesByArch.set(arch, [...(nodesByArch.get(arch) ?? []), node.name]);
  }

  return (
    <div>
      <h1>
        <Icon name="refresh" /> Self-update
      </h1>

      <div className="card section">
        <div className="section-head">
          <h2>Release settings</h2>
        </div>
        <p>
          Point clouduler at a GitHub repo to track its latest release, or at one release's own page to pin it. Each
          node downloads its own <code>clouduler-&lt;arch&gt;-unknown-linux-gnu</code> binary from that release
          directly. Further changes after the release url is set require editing the config file directly.
        </p>

        <div className="field">
          <label htmlFor="self-update-release-url">Release URL</label>
          <input
            id="self-update-release-url"
            type="text"
            placeholder={DEFAULT_RELEASE_URL}
            value={repo?.release_url ?? releaseUrl}
            onChange={(e) => setReleaseUrl(e.target.value)}
            disabled={!!repo || !isAdmin}
          />
        </div>

        {isAdmin && !repo && (
          <button className="primary" onClick={saveRepo} disabled={busy || !releaseUrl.trim()}>
            <Icon name="check" /> Save release
          </button>
        )}
        {repo && (
          <p>
            <Badge tone="good">configured</Badge>
          </p>
        )}

        <div className="field">
          <label htmlFor="self-update-github-token">GitHub API token (optional)</label>
          <input
            id="self-update-github-token"
            type="password"
            placeholder={githubToken?.configured ? 'already configured' : 'needed for a private repo or higher rate limits'}
            value={tokenInput}
            onChange={(e) => setTokenInput(e.target.value)}
            disabled={!isAdmin}
          />
        </div>
        {isAdmin && (
          <button onClick={saveToken} disabled={busy || !tokenInput.trim()}>
            <Icon name="key" /> Save token
          </button>
        )}
        {githubToken?.configured && (
          <p>
            <Badge tone="good">token configured</Badge>
          </p>
        )}

        {error && (
          <p className="field-error">
            <Icon name="alert-circle" /> {error}
          </p>
        )}
      </div>

      <div className="section">
        <div className="section-head">
          <h2>Update progress</h2>
          {isAdmin && (
            <div>
              {inProgress ? (
                <button className="danger" onClick={cancelUpdate}>
                  <Icon name="close" /> Cancel
                </button>
              ) : (
                <button className="primary" onClick={startUpdate} disabled={busy || !repo}>
                  <Icon name="refresh" /> Start update
                </button>
              )}
            </div>
          )}
        </div>

        {!repo && <p>Configure a release above before starting an update.</p>}

        {run ? (
          <>
            {run.release_tag && (
              <p>
                Rolling out <Badge tone="neutral">{run.release_tag}</Badge>
              </p>
            )}
            {run.failure_reason && (
              <p className="banner banner-warn">
                <Icon name="alert" /> {run.failure_reason}
              </p>
            )}
            {Array.from(nodesByArch.entries()).map(([arch, names]) => (
              <div key={arch} className="table-wrap">
                <h3>{arch}</h3>
                <table>
                  <thead>
                    <tr>
                      <th>Node</th>
                      <th>Phase</th>
                    </tr>
                  </thead>
                  <tbody>
                    {names.map((name) => {
                      const phase = run.per_node[name];
                      return (
                        <tr key={name}>
                          <td>{name}</td>
                          <td>{phase ? <Badge tone={phaseTone(phase)}>{phaseLabel(phase)}</Badge> : '-'}</td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            ))}
          </>
        ) : (
          <p>No update has been started yet.</p>
        )}
      </div>
    </div>
  );
}
