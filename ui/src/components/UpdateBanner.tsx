// Shown on every page when the leader's periodic release check has found a
// tag that some node is not running, with a button that starts the rollout.

import { useState } from 'react';
import { api, ApiError } from '../api/client';
import type { SelfUpdateCheck, SelfUpdateRunState } from '../api/types';
import { useAuth } from '../auth/AuthContext';
import { Banner, useConfirm, useToast } from '../ds';
import { useClusterState } from '../state/store';

const CHECK_CONFIG = '_clouduler_self_update_check';
const RUN_CONFIG = '_clouduler_self_update_run';
const DISMISSED_KEY = 'clouduler.dismissedUpdate';

function decode<T>(content: string | undefined): T | null {
  if (!content) return null;
  try {
    return JSON.parse(atob(content)) as T;
  } catch {
    return null;
  }
}

const bare = (v: string) => v.replace(/^v/, '');

export function UpdateBanner() {
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const confirm = useConfirm();
  const toast = useToast();
  const [dismissed, setDismissed] = useState(() => sessionStorage.getItem(DISMISSED_KEY));
  const [busy, setBusy] = useState(false);

  const check = decode<SelfUpdateCheck>(cluster?.configs[CHECK_CONFIG]?.content);
  const run = decode<SelfUpdateRunState>(cluster?.configs[RUN_CONFIG]?.content);
  const tag = check?.tag;
  if (!cluster || !tag || dismissed === tag) return null;
  if (run && !run.finished_at) return null;
  // A node that has not reported a version yet is not evidence of anything.
  const behind = Object.values(cluster.nodes).some((n) => n.version && bare(n.version) !== bare(tag));
  if (!behind) return null;

  const start = async () => {
    const ok = await confirm({
      title: `Update to ${tag}?`,
      body: 'Nodes download the release, then restart one at a time once all are ready.',
      confirmLabel: `Update all nodes to ${tag}`,
      confirmVariant: 'primary',
    });
    if (!ok) return;
    setBusy(true);
    try {
      await api.startSelfUpdate();
      toast(`Updating to ${tag}.`);
    } catch (e) {
      toast(e instanceof ApiError ? e.message : 'Could not start the update.');
    } finally {
      setBusy(false);
    }
  };

  return (
    <Banner
      tone="info"
      title={`Clouduler ${tag} is available`}
      action={isAdmin && !busy ? 'Update all nodes' : undefined}
      onAction={() => void start()}
      onClose={() => {
        sessionStorage.setItem(DISMISSED_KEY, tag);
        setDismissed(tag);
      }}
    >
      Some nodes are on an older version.
    </Banner>
  );
}
