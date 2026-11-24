import { CheckboxField } from '../../../components/FormFields';
import type { SectionProps } from './types';

export function McpSection({ draft, setDraft, disabled }: SectionProps) {
  return (
    <div className="card">
      <CheckboxField
        id="mcp-enabled"
        label="Allow AI clients"
        value={draft.mcp.enabled}
        onChange={(enabled) => setDraft({ ...draft, mcp: { ...draft.mcp, enabled } })}
        hint="Claude Code, Cursor and similar tools."
        help="Serves the /mcp endpoint and the OAuth pages that let tools such as Claude Code, Cursor or claude.ai connect after a user approves them. Turning it off hides all of it at once; clients that were already approved can still be listed and revoked on the AI access page. Applies live."
        configKey="mcp.enabled"
        disabled={disabled}
      />
    </div>
  );
}
