import type { ClusterConfig } from '../../../api/types';
import { Icon } from '../../../components/Icon';
import { DurationField } from '../../../components/FormFields';
import { fenceMarginWarning } from '../../../lib/validate';
import type { SectionProps } from './types';

export function FailoverSection({ draft, setDraft, disabled }: SectionProps) {
  const f = draft.failover;
  const set = (patch: Partial<ClusterConfig['failover']>) => setDraft({ ...draft, failover: { ...f, ...patch } });
  const marginWarning = fenceMarginWarning(f);
  return (
    <div className="card card--grid">
      <DurationField
        id="fo-node"
        label="Declare a node lost after"
        value={f.node_timeout}
        onChange={(node_timeout) => set({ node_timeout })}
        hint="e.g. 30s. Longer than the fence timeout."
        help="How long a node can go unreachable before the leader marks it unreachable and reschedules its work elsewhere, e.g. 30s. Must be longer than the fence timeout, so a node has fenced itself before the rest of the cluster gives up on it."
        configKey="failover.node_timeout"
        disabled={disabled}
      />
      <DurationField
        id="fo-fence"
        label="Stop containers without quorum after"
        value={f.fence_timeout}
        onChange={(fence_timeout) => set({ fence_timeout })}
        disabled={disabled}
        hint="e.g. 15s. Shorter than the node timeout."
        help="How long a node can go without raft quorum before it stops its own containers, e.g. 15s. Must be shorter than the node timeout and longer than the probe interval; this ordering is what stops a replicated volume from ending up with two writers."
        configKey="failover.fence_timeout"
      />
      <DurationField
        id="fo-probe"
        label="Check nodes every"
        value={f.probe_interval}
        onChange={(probe_interval) => set({ probe_interval })}
        hint="e.g. 5s. Shorter than the fence timeout."
        help="How often the leader probes every node's reachability, e.g. 5s. Must be shorter than the fence timeout, so a node cannot be declared dead before it has had the chance to fence itself."
        configKey="failover.probe_interval"
        disabled={disabled}
      />
      <DurationField
        id="fo-grace"
        label="Container stop grace period"
        value={f.stop_grace}
        onChange={(stop_grace) => set({ stop_grace })}
        hint="e.g. 30s. Applies live."
        help="How long a container gets to stop cleanly before it is killed, e.g. 30s, for services that do not set their own stop_grace_period. Counts against the gap between the fence timeout and the node timeout. Applies live."
        configKey="failover.stop_grace"
        disabled={disabled}
      />
      <DurationField
        id="fo-push"
        label="Shutdown volume push time limit"
        value={f.shutdown_push_timeout}
        onChange={(shutdown_push_timeout) => set({ shutdown_push_timeout })}
        hint="e.g. 15m. 0s skips the push. At most 2h."
        help="When a node's machine is rebooting or powering off, the node first pushes the volumes it leads to at least one other node, most important stack first, for up to this long, e.g. 15m. 0s skips the push. A node that is the cluster leader also gives up leadership first, whatever this is set to. Read when the shutdown starts, so a change applies to the next one."
        configKey="failover.shutdown_push_timeout"
        disabled={disabled}
      />
      {marginWarning && (
        <div className="banner banner-warn" role="alert">
          <Icon name="alert-circle" /> {marginWarning}
        </div>
      )}
    </div>
  );
}
