import type { ClusterConfig } from '../../../api/types';
import { CheckboxField } from '../../../components/FormFields';
import type { SectionProps } from './types';

// The key file path is deliberately not editable here. Repointing it from the
// ui is never what someone means: the key it names is what opens every stored
// secret, and a wrong path silently produces a node that replicates secrets it
// cannot decrypt. Moving it is a deliberate, on-the-box operation. The value
// still rides along in the draft, so it survives an edit made from this page.
export function SecretsSection({ draft, setDraft, disabled }: SectionProps) {
  const s = draft.secrets;
  const set = (patch: Partial<ClusterConfig['secrets']>) => setDraft({ ...draft, secrets: { ...s, ...patch } });
  return (
    <div className="card">
      <CheckboxField
        id="sec-genmissing"
        label="Create the cluster key if missing"
        value={s.generate_if_missing}
        onChange={(generate_if_missing) => set({ generate_if_missing })}
        hint="Each node can override this."
        help="Cluster-wide default for whether a node may create the secret-sealing key if it doesn't have one yet. Each node can override this under its own Nodes page card, e.g. to forbid it on a node joining an existing cluster."
        configKey="secrets.generate_if_missing"
        disabled={disabled}
      />
    </div>
  );
}
