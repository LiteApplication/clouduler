// Pieces the stack detail and deploy screens share: the names the spec editor
// completes against, the cluster context placement needs, and the config and
// secret key sets a spec references.

import { useEffect, useMemo, useState } from 'react';
import { api } from '../../api/client';
import type { StackRequirements } from '../../api/types';
import { useClusterState } from '../../state/store';
import { collectImages, emptyKnownNames, type KnownNames } from '../../lib/stackSchema';
import type { PlacementContext } from '../../lib/placement';

/** Everything this cluster already knows the name of, for the spec editor's
 *  live-data autocomplete and ctrl+click navigation. */
export function useKnownNames(): KnownNames {
  const cluster = useClusterState();
  return useMemo(() => {
    if (!cluster) return emptyKnownNames;
    const networks = new Set<string>();
    const drives = new Set<string>();
    for (const node of Object.values(cluster.nodes)) {
      for (const n of node.networks) networks.add(n);
      for (const d of Object.keys(node.drives)) drives.add(d);
    }
    return {
      networks: Array.from(networks).sort(),
      drives: Array.from(drives).sort(),
      nodes: Object.keys(cluster.nodes).sort(),
      instances: Object.keys(cluster.assignments),
      secrets: Object.keys(cluster.secrets).sort(),
      configs: Object.keys(cluster.configs).sort(),
      volumes: Object.keys(cluster.volumes).sort(),
      images: collectImages(Object.values(cluster.stacks).map((s) => s.spec)),
    };
  }, [cluster]);
}

/** `configs` (service-referenced) and `file_configs` (declared with a bundled
 *  `file:`) aren't subsets of one another, so the editable set is their union. */
export function allConfigKeys(requirements: StackRequirements): string[] {
  return Array.from(new Set([...requirements.configs, ...requirements.file_configs])).sort();
}

export function allSecretKeys(requirements: StackRequirements): string[] {
  return Array.from(new Set([...requirements.secrets, ...requirements.file_secrets])).sort();
}

const EMPTY_CTX: PlacementContext = {
  servedDrives: new Set(),
  natNetworks: new Set(),
  driveHolders: {},
  portGatedNetworks: new Set(),
};

/** The extra cluster facts `lib/placement.ts` needs to give the same "can
 *  schedule here" answer the real scheduler would. Falls back to strict
 *  per-node eligibility until (or unless) they load. */
export function usePlacementContext(): PlacementContext {
  const [ctx, setCtx] = useState<PlacementContext>(EMPTY_CTX);

  useEffect(() => {
    let cancelled = false;
    Promise.all([api.listDrives().catch(() => null), api.getConfiguration().catch(() => null)]).then(
      ([drives, config]) => {
        if (cancelled) return;
        setCtx({
          servedDrives: new Set((drives ?? []).filter((d) => d.mode === 'served').map((d) => d.name)),
          driveHolders: Object.fromEntries(
            (drives ?? []).map((d) => [d.name, d.mode === 'served' ? d.served_by : d.nodes.map((n) => n.node)]),
          ),
          natNetworks: new Set(
            Object.entries(config?.networks ?? {})
              .filter(([, net]) => net.type === 'nat')
              .map(([name]) => name),
          ),
          portGatedNetworks: new Set(
            Object.entries(config?.networks ?? {})
              .filter(([, net]) => net.type === 'public' || net.type === 'wireguard')
              .map(([name]) => name),
          ),
        });
      },
    );
    return () => {
      cancelled = true;
    };
  }, []);

  return ctx;
}

export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}
