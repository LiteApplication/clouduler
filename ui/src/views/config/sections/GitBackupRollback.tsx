import { useCallback, useEffect, useState } from "react";
import { api, ApiError } from "../../../api/client";
import type { GitBackupCommit, GitBackupResetRelease } from "../../../api/types";
import { useAuth } from "../../../auth/AuthContext";
import { Banner, useConfirm, useToast } from "../../../ds";
import { Icon } from "../../../components/Icon";
import { relativeTime } from "../../../lib/format";

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
  const [mode, setMode] = useState<"revert" | "reset">("revert");
  const [hash, setHash] = useState("");
  // "auto" | "current" | "custom" | a release tag from the list.
  const [releaseChoice, setReleaseChoice] = useState("auto");
  const [customTag, setCustomTag] = useState("");
  const [releases, setReleases] = useState<string[] | null>(null);
  const [releasesFailed, setReleasesFailed] = useState(false);
  const hashValid = /^[0-9a-f]{4,40}$/i.test(hash.trim());
  const typing = releasesFailed || releaseChoice === "custom";
  const resolvedRelease: GitBackupResetRelease | null = typing
    ? customTag.trim() === ""
      ? null
      : { mode: "tag", tag: customTag.trim() }
    : releaseChoice === "auto"
      ? { mode: "auto" }
      : releaseChoice === "current"
        ? { mode: "current" }
        : { mode: "tag", tag: releaseChoice };
  const releaseText = !resolvedRelease
    ? "(no release chosen)"
    : resolvedRelease.mode === "auto"
      ? "the release recorded at that commit (the running one if none is recorded)"
      : resolvedRelease.mode === "current"
        ? "the running release (nothing is downloaded)"
        : `release ${resolvedRelease.tag}`;

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setCommits(await api.gitBackupCommits());
    } catch (e) {
      setError(e instanceof ApiError ? e.message : "failed to list commits");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (open && commits === null && !loading) void load();
  }, [open, commits, loading, load]);

  useEffect(() => {
    if (!open || mode !== "reset" || releases !== null || releasesFailed) return;
    api
      .selfUpdateReleases()
      .then((r) => setReleases(r.tags))
      .catch(() => setReleasesFailed(true));
  }, [open, mode, releases, releasesFailed]);

  if (!configured || !isAdmin) return null;

  const rollback = async (c: GitBackupCommit) => {
    const ok = await confirm({
      title: `Roll back the configuration to ${c.short}?`,
      body: "This pushes a new commit to your repository and applies it to the running cluster: stacks that differ are redeployed and stacks absent from that commit are removed.",
      confirmLabel: "Roll back",
      confirmVariant: "danger",
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
      setError(e instanceof ApiError ? e.message : "rollback failed");
    } finally {
      setBusy(false);
    }
  };

  const reset = async () => {
    const commit = hash.trim().toLowerCase();
    const short = commit.slice(0, 12);
    if (!resolvedRelease) return;
    const ok = await confirm({
      title: `Reset the cluster to ${short}?`,
      body: `Everything after this commit is moved to a rolled-back/ branch and the main branch is force-pushed back to it. Installed release: ${releaseText}. Every node downloads it, quorum is broken, the configuration is set from that commit without history checks, and ALL nodes reboot at the same time. Expect downtime.`,
      confirmLabel: "Reset and reboot all nodes",
      confirmVariant: "danger",
      requireText: short,
    });
    if (!ok) return;
    setBusy(true);
    setError(null);
    try {
      const r = await api.gitBackupReset(commit, resolvedRelease);
      toast(
        `Reset to ${r.commit.slice(0, 12)}; previous history saved on branch ${r.branch}.`,
      );
      setHash("");
      await load();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : "reset failed");
    } finally {
      setBusy(false);
    }
  };

  return (
    <details
      style={{ marginTop: 24 }}
      onToggle={(e) => setOpen((e.currentTarget as HTMLDetailsElement).open)}
    >
      <summary className="muted" style={{ cursor: "pointer" }}>
        Advanced: roll back the configuration to a commit
      </summary>
      <div style={{ marginTop: 12 }}>
        <div
          className="btn-row"
          role="group"
          aria-label="Rollback method"
          style={{ margin: "8px 0" }}
        >
          <button
            type="button"
            aria-pressed={mode === "revert"}
            onClick={() => setMode("revert")}
          >
            Revert to a commit
          </button>
          <button
            type="button"
            aria-pressed={mode === "reset"}
            onClick={() => setMode("reset")}
          >
            Reset to commit (rewrites history)
          </button>
        </div>
        {mode === "reset" && (
          <div>
            <Banner
              tone="danger"
              title="Rewrites history and reboots every node"
            >
              Everything after the chosen commit is moved to a rolled-back/
              branch and the main branch is force-pushed back to it. All nodes
              download the chosen release, quorum is broken, the configuration is
              set from that commit without checking git history, and ALL nodes
              reboot at the same time (downtime).
            </Banner>
            <label
              className="field"
              style={{ display: "block", margin: "8px 0" }}
            >
              <span>Config commit hash</span>
              <input
                type="text"
                aria-label="Config commit hash"
                value={hash}
                onChange={(e) => setHash(e.target.value)}
                placeholder="sha or prefix (at least 4 hex chars)"
                spellCheck={false}
              />
            </label>
            {hash !== "" && !hashValid && (
              <p className="field-hint">
                Enter 4 to 40 hexadecimal characters.
              </p>
            )}
            <label
              className="field"
              style={{ display: "block", margin: "8px 0" }}
            >
              <span>Release</span>
              {!releasesFailed && (
                <select
                  aria-label="Release"
                  value={releaseChoice}
                  onChange={(e) => setReleaseChoice(e.target.value)}
                >
                  <option value="auto">
                    Auto (version recorded at that commit)
                  </option>
                  <option value="current">Keep the running release</option>
                  {(releases ?? []).map((t) => (
                    <option key={t} value={t}>
                      {t}
                    </option>
                  ))}
                  <option value="custom">Other (type a tag)...</option>
                </select>
              )}
            </label>
            {releasesFailed && (
              <p className="field-hint">
                Could not load the release list; type a tag.
              </p>
            )}
            {typing && (
              <label
                className="field"
                style={{ display: "block", margin: "8px 0" }}
              >
                <span>Release tag</span>
                <input
                  type="text"
                  aria-label="Release tag"
                  value={customTag}
                  onChange={(e) => setCustomTag(e.target.value)}
                  placeholder="e.g. v0.18.0"
                  spellCheck={false}
                />
              </label>
            )}
            <p className="field-hint">Will install: {releaseText}.</p>
            <div className="btn-row">
              <button
                type="button"
                className="danger"
                disabled={!hashValid || !resolvedRelease || busy}
                onClick={() => void reset()}
              >
                Reset to this commit
              </button>
            </div>
          </div>
        )}
        {mode === "revert" && (
          <span className="field-hint">
            Creates a new commit on top of the branch whose files equal the
            chosen commit's, then the cluster applies it like a pull: stacks
            that differ are redeployed, stacks absent from that commit are
            removed, and configs and cluster.yaml are restored. Secrets and
            terraform state are never rolled back. The commit is pushed to your
            repository; history is not rewritten.
          </span>
        )}
        {mode === "revert" && (
          <Banner tone="warn" title="Use with care">
            A rollback can remove running stacks and change the cluster
            configuration. Data in removed stacks' volumes may not be
            recoverable from git.
          </Banner>
        )}
        <div className="btn-row" style={{ margin: "8px 0" }}>
          <button
            type="button"
            onClick={() => void load()}
            disabled={loading || busy}
          >
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
        {commits && commits.length === 0 && (
          <p className="muted">No commits found.</p>
        )}
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
                    {mode === "reset" ? (
                      <button
                        type="button"
                        onClick={() => setHash(c.sha)}
                        disabled={busy}
                        aria-label={`Use ${c.short}`}
                      >
                        Use this commit
                      </button>
                    ) : (
                      <button
                        type="button"
                        onClick={() => void rollback(c)}
                        disabled={busy}
                        aria-label={`Roll back to ${c.short}`}
                      >
                        Roll back to this
                      </button>
                    )}
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
