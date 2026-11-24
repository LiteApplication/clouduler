import type { ClusterConfig } from '../../../api/types';

export interface SectionProps {
  draft: ClusterConfig;
  setDraft: (c: ClusterConfig) => void;
  disabled: boolean;
  /** The config as last fetched from the cluster, before any staged edits.
   *  Lets a section tell an already-applied network from one that only
   *  exists in the unsaved draft (e.g. just added by "Add network"), which
   *  a rename against the live `/api/config/networks/{name}/rename` cannot
   *  know about yet. */
  original: ClusterConfig;
}
