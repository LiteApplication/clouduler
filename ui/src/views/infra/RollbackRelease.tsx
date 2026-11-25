// Settings > Updates: the deliberately buried, unsupported way back to an older release.

import { useEffect, useState } from 'react';
import { api, ApiError } from '../../api/client';
import { Banner, Button, useConfirm, useToast } from '../../ds';
import { bare, isNewer } from '../../components/version';

export function RollbackRelease() {
  const confirm = useConfirm();
  const toast = useToast();
  const [running, setRunning] = useState<string | null>(null);
  const [tags, setTags] = useState<string[] | null>(null);
  const [tag, setTag] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.selfUpdateLatest().then((l) => setRunning(l.running_version)).catch(() => {});
    api.selfUpdateReleases().then((r) => setTags(r.tags)).catch(() => setTags([]));
  }, []);

  const older = running && tags ? tags.filter((t) => isNewer(running, t)) : [];
  const chosen = older.includes(tag) ? tag : '';

  const go = async () => {
    setError(null);
    const ok = await confirm({
      title: `Roll back to ${chosen}?`,
      body: (
        <p>
          This is not supported. Nodes will most probably fail to start on {chosen}, because it cannot read the newer
          cluster state. Nothing restores the data afterwards. Type the release tag to continue.
        </p>
      ),
      confirmLabel: `Roll back to ${chosen}`,
      confirmVariant: 'danger',
      requireText: chosen,
    });
    if (!ok) return;
    setBusy(true);
    try {
      await api.startSelfUpdate(chosen, { downgrade: true });
      toast(`Started the rollback to ${chosen}. Follow it on the Updates page.`);
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'Could not start the rollback.');
    } finally {
      setBusy(false);
    }
  };

  return (
    <details className="al-adv">
      <summary>Advanced: roll back to an older release (unsupported)</summary>
      <div className="stack">
        <Banner tone="danger" title="Not supported: this will most probably break your cluster">
          <p>
            Older binaries probably cannot read the newer cluster state or database schema, so the nodes will most
            probably fail to start. The per-node local rollback only helps if the new binary crashes; it does not help
            if it runs but cannot read the data.
          </p>
          <p>
            Take a git config backup and drive and database backups first. If the rollback fails part-way, the cluster
            is left on mixed versions. Never do this on a production cluster you cannot afford to lose.
          </p>
        </Banner>
        {error && <Banner tone="danger" onClose={() => setError(null)}>{error}</Banner>}
        {tags && older.length === 0 ? (
          <p className="cl-sm cl-muted">No older release is available.</p>
        ) : (
          <>
            <label className="cl-sm">
              Older release{' '}
              <select aria-label="Older release" value={chosen} onChange={(e) => setTag(e.target.value)}>
                <option value="">Select a release</option>
                {older.map((t) => <option key={t} value={t}>{t}</option>)}
              </select>
            </label>
            <div>
              <Button variant="danger" disabled={busy || !chosen} onClick={() => void go()}>
                {chosen ? `Roll back to ${bare(chosen)}` : 'Roll back'}
              </Button>
            </div>
          </>
        )}
      </div>
    </details>
  );
}
