import { useEffect, useState } from 'react';
import { api, ApiError } from '../api/client';
import type { SelfUpdateConfig, SelfUpdateNodePhase, SelfUpdateRunState, SelfUpdateSshKeyStatus } from '../api/types';
import { useClusterState } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { Badge } from '../components/Badge';
import type { BadgeTone } from '../components/Badge';
import { Icon } from '../components/Icon';

const RUN_CONFIG = '_clouduler_self_update_run';

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
  if (key === 'built') return `built ${(value as { sha256: string }).sha256.slice(0, 8)}`;
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

  const [sshKey, setSshKey] = useState<SelfUpdateSshKeyStatus | null>(null);
  const [repo, setRepo] = useState<SelfUpdateConfig | null>(null);
  const [generatedPublicKey, setGeneratedPublicKey] = useState<string | null>(null);
  const [gitUrl, setGitUrl] = useState('');
  const [gitBranch, setGitBranch] = useState('main');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.selfUpdateSshKey().then(setSshKey).catch(() => {});
    api.selfUpdateRepo().then(setRepo).catch(() => {});
  }, []);

  const publicKey = generatedPublicKey ?? sshKey?.public_key ?? null;

  const generateKey = async () => {
    setError(null);
    setBusy(true);
    try {
      const result = await api.generateSelfUpdateSshKey();
      setGeneratedPublicKey(result.public_key);
      setSshKey({ public_key: result.public_key });
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'failed to generate a deploy key');
    } finally {
      setBusy(false);
    }
  };

  const saveRepo = async () => {
    if (!publicKey) return;
    setError(null);
    setBusy(true);
    try {
      await api.setSelfUpdateRepo({ git_url: gitUrl.trim(), git_branch: gitBranch.trim(), ssh_public_key: publicKey });
      setRepo(await api.selfUpdateRepo());
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'failed to save the repo');
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
          <h2>Repo settings</h2>
        </div>
        <p>
          Generate a deploy key first, add it as a read-only deploy key on the git host, then set the repo. Further
          changes after that require editing the config file directly.
        </p>

        {publicKey ? (
          <div className="field">
            <label>Deploy public key</label>
            <textarea readOnly rows={3} value={publicKey} className="mono" />
          </div>
        ) : (
          isAdmin && (
            <button onClick={generateKey} disabled={busy}>
              <Icon name="key" /> Generate deploy key
            </button>
          )
        )}

        <div className="field">
          <label htmlFor="self-update-git-url">Git URL</label>
          <input
            id="self-update-git-url"
            type="text"
            placeholder="git@example.com:org/repo.git"
            value={repo?.git_url ?? gitUrl}
            onChange={(e) => setGitUrl(e.target.value)}
            disabled={!!repo || !isAdmin}
          />
        </div>
        <div className="field">
          <label htmlFor="self-update-git-branch">Branch</label>
          <input
            id="self-update-git-branch"
            type="text"
            value={repo?.git_branch ?? gitBranch}
            onChange={(e) => setGitBranch(e.target.value)}
            disabled={!!repo || !isAdmin}
          />
        </div>

        {isAdmin && !repo && (
          <button className="primary" onClick={saveRepo} disabled={busy || !publicKey || !gitUrl.trim()}>
            <Icon name="check" /> Save repo
          </button>
        )}
        {repo && (
          <p>
            <Badge tone="good">configured</Badge>
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

        {!repo && <p>Configure a repo above before starting an update.</p>}

        {run ? (
          <>
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
                      <th>Role</th>
                      <th>Phase</th>
                    </tr>
                  </thead>
                  <tbody>
                    {names.map((name) => {
                      const phase = run.per_node[name];
                      const isBuilder = run.arch_builders[arch] === name;
                      return (
                        <tr key={name}>
                          <td>{name}</td>
                          <td>{isBuilder ? 'builder' : ''}</td>
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
