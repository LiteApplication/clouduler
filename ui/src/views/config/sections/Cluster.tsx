import { TextField } from '../../../components/FormFields';
import type { SectionProps } from './types';

export function ClusterSection({ draft, setDraft, disabled }: SectionProps) {
  return (
    <div className="card">
      <TextField
        id="cluster-name"
        label="Cluster name"
        value={draft.cluster.name}
        onChange={(name) => setDraft({ ...draft, cluster: { ...draft.cluster, name } })}
        hint="Shown across the UI."
        help="Shown in the UI and used as the default suffix for internal DNS names."
        configKey="cluster.name"
        disabled={disabled}
      />
    </div>
  );
}
