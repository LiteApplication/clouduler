import { useState } from 'react';
import { api, ApiError } from '../../api/client';
import { Button, useConfirm, useToast } from '../../ds';

/** Time-sensitive operator override: rendered only while a move is verifying. */
export function SkipVerifyButton({
  instanceId,
  phase,
  isAdmin,
}: {
  instanceId: string;
  phase: string | null;
  isAdmin: boolean;
}) {
  const confirm = useConfirm();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  if (!isAdmin || phase !== 'verifying') return null;

  const skip = async () => {
    const ok = await confirm({
      title: 'Skip verification?',
      body: 'The copy on the target node will not be checked against the source before the move switches over. Any file that did not transfer correctly is lost when the source is released. This cannot be undone.',
      confirmLabel: 'Skip verification',
      confirmVariant: 'danger',
    });
    if (!ok) return;
    setBusy(true);
    try {
      await api.skipMigrationVerify(instanceId);
      toast(`Skipping verification of ${instanceId}`);
    } catch (e) {
      toast('Could not skip verification', { tone: 'danger', body: e instanceof ApiError ? e.message : String(e) });
    } finally {
      setBusy(false);
    }
  };

  return (
    <Button size="sm" variant="danger" loading={busy} onClick={skip}>
      Skip verification
    </Button>
  );
}
