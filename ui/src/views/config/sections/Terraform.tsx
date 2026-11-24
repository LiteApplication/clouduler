import type { ClusterConfig } from '../../../api/types';
import { CheckboxField, DurationField, TextField } from '../../../components/FormFields';
import type { SectionProps } from './types';

const DEFAULT_TERRAFORM: NonNullable<ClusterConfig['terraform']> = {
  image: 'ghcr.io/opentofu/opentofu:1.10',
  timeout: '10m',
  init_network: 'bridge',
};

export function TerraformSection({ draft, setDraft, disabled }: SectionProps) {
  const t = draft.terraform ?? null;
  const set = (patch: Partial<NonNullable<ClusterConfig['terraform']>>) =>
    setDraft({ ...draft, terraform: t ? { ...t, ...patch } : { ...DEFAULT_TERRAFORM, ...patch } });

  return (
    <div className="card">
      <CheckboxField
        id="terraform-enabled"
        label="Run OpenTofu modules"
        value={t != null}
        onChange={(enabled) => setDraft({ ...draft, terraform: enabled ? { ...DEFAULT_TERRAFORM } : null })}
        hint="Per-service modules, applied after a deploy."
        help="Lets stacks keep an OpenTofu module per service that configures the running service through its API, e.g. creating monitors or users. The leader runs it in a one-off container inside the service's network namespace, and keeps the state itself. Off by default; applies live."
        configKey="terraform"
        disabled={disabled}
      />
      {t && (
        <>
          <TextField
            id="terraform-image"
            label="OpenTofu image"
            value={t.image}
            onChange={(image) => set({ image })}
            hint="Used for init, plan and apply."
            help="The container image every terraform run uses. The default is the official OpenTofu image; pin a tag you have tested."
            configKey="terraform.image"
            disabled={disabled}
          />
          <DurationField
            id="terraform-timeout"
            label="Run timeout"
            value={t.timeout}
            onChange={(timeout) => set({ timeout })}
            hint="e.g. 10m. Wall-clock limit for one run."
            help="A run that takes longer than this is stopped and reported as failed."
            configKey="terraform.timeout"
            disabled={disabled}
          />
          <TextField
            id="terraform-init-network"
            label="Init network"
            value={t.init_network}
            onChange={(init_network) => set({ init_network })}
            hint="Docker network used to download providers."
            help="Docker network the tofu init container runs on, so it can reach the provider registry. Plan and apply run inside the service's own network namespace instead."
            configKey="terraform.init_network"
            disabled={disabled}
          />
        </>
      )}
    </div>
  );
}
