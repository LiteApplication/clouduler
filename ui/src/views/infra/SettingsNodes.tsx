// /settings/nodes/<node>: one sub-tab per node, each a compact header plus
// collapsible General, Networks and Drives sections. Edits go into the shared
// config draft and are applied with the Settings header's "Apply changes".

import { Link, Navigate, useNavigate } from 'react-router-dom';
import { useAuth } from '../../auth/AuthContext';
import { Banner, Tabs, useConfirm } from '../../ds';
import { NodeCard } from '../config/sections';
import type { SettingsConfig } from './SettingsConfig';

export function SettingsNodes({ cfg, node }: { cfg: SettingsConfig; node?: string }) {
  const { isAdmin } = useAuth();
  const confirm = useConfirm();
  const navigate = useNavigate();
  const { draft, setDraft } = cfg;
  if (!draft) return null;
  const names = Object.keys(draft.nodes);
  if (names.length === 0) return <Banner tone="warn" title="No nodes in the cluster config">Add a node from the Nodes page.</Banner>;
  if (!node || !(node in draft.nodes)) return <Navigate to={`/settings/nodes/${encodeURIComponent(names[0])}`} replace />;
  const configNode = draft.nodes[node];

  return (
    <div className="stack">
      <Tabs
        label="Nodes"
        items={names.map((n) => ({ id: n, label: n }))}
        active={node}
        onChange={(n) => navigate(`/settings/nodes/${encodeURIComponent(n)}`)}
      />
      <NodeCard
        key={node}
        name={node}
        node={configNode}
        networks={draft.networks}
        drives={draft.drives}
        cluster={draft}
        onChange={(patch) => setDraft({ ...draft, nodes: { ...draft.nodes, [node]: { ...configNode, ...patch } } })}
        onRemove={async () => {
          if (!(await confirm({ title: `Remove ${node} from the cluster config?`, body: 'This does not remove the node itself.', confirmLabel: 'Remove from config', confirmVariant: 'danger' }))) return;
          const next = { ...draft.nodes };
          delete next[node];
          setDraft({ ...draft, nodes: next });
        }}
        disabled={!isAdmin || cfg.busy}
      />
      <p className="sub">
        <Link to={`/nodes/${encodeURIComponent(node)}`}>Open the {node} node page</Link> for health, resources and logs.
      </p>
    </div>
  );
}
