import { useCallback, useEffect, useState } from 'react';
import { api, ApiError } from '../../../api/client';
import type { GitBackupCommit } from '../../../api/types';
import { useAuth } from '../../../auth/AuthContext';
import { Banner, useConfirm, useToast } from '../../../ds';
import { Icon } from '../../../components/Icon';
import { relativeTime } from '../../../lib/format';

/** Deliberately low-key: a collapsed section for admins, shown only once git
 *  backup is configured. Rolling back adds a NEW commit on top of the branch
 *  (history is never rewritten) and applies it like a pull. */
export function GitBackupRollback({ configured }: { configured: boolean }) {
  const { isAdmin } = useAuth();
  const confirm = useConfirm();
  const toast = useToast();
  const [open, setOpen] = useState(false);
  const [commits, setCommits] = useState<GitBackupCommit[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setCommits(await api.gitBackupCommits());
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'failed to list commits');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (open && commits === null && !loading) void load();
  }, [open, commits, loading, load]);

  if (!configured || !isAdmin) return null;

  const rollback = async (c: GitBackupCommit) => {
    const ok = await confirm({
      title: `Roll back the configuration to ${c.short}?`,
      body: 'This pushes a new commit to your repository and applies it to the running cluster: stacks that differ are redeployed and stacks absent from that commit are removed.',
      confirmLabel: 'Roll back',
      confirmVariant: 'danger',
      requireText: c.short,
    });
    if (!ok) return;
    setBusy(true);
    setError(null);
    try {
      const r = await api.gitBackupRollback(c.sha);
      toast(`Rolled back to ${c.short}; new commit ${r.commit.slice(0, 12)}.`);
      await load();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'rollback failed');
    } finally {
      setBusy(false);
    }
  };

  return (
    <details style={{ marginTop: 24 }} onToggle={(e) => setOpen((e.currentTarget as HTMLDetailsElement).open)}>
      <summary className="muted" style={{ cursor: 'pointer' }}>
        Advanced: roll back the configuration to a commit
      </summary>
      <div style={{ marginTop: 12 }}>
        <span className="field-hint">
          Creates a new commit on top of the branch whose files equal the chosen commit's, then the cluster applies it
          like a pull: stacks that differ are redeployed, stacks absent from that commit are removed, and configs and
          cluster.yaml are restored. Secrets and terraform state are never rolled back. The commit is pushed to your
          repository; history is not rewritten.
        </span>
        <Banner tone="warn" title="Use with care">
          A rollback can remove running stacks and change the cluster configuration. Data in removed stacks' volumes
          may not be recoverable from git.
        </Banner>
        <div className="btn-row" style={{ margin: '8px 0' }}>
          <button type="button" onClick={() => void load()} disabled={loading || busy}>
            <Icon name="refresh" /> Refresh
          </button>
        </div>
        {error && (
          <Banner tone="danger" title="Rollback failed">
            {error}
          </Banner>
        )}
        {loading && commits === null && (
          <p className="loading">
            <span className="spinner" /> Loading commits...
          </p>
        )}
        {commits && commits.length === 0 && <p className="muted">No commits found.</p>}
        {commits && commits.length > 0 && (
          <table>
            <tbody>
              {commits.map((c) => (
                <tr key={c.sha}>
                  <td>
                    <code>{c.short}</code>
                  </td>
                  <td>{c.message}</td>
                  <td className="muted">{c.author}</td>
                  <td className="muted">{relativeTime(c.time)}</td>
                  <td>
                    <button type="button" onClick={() => void rollback(c)} disabled={busy} aria-label={`Roll back to ${c.short}`}>
                      Roll back to this
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </details>
  );
}
