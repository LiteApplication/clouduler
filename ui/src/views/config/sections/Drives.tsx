import { useState } from 'react';
import { api, ApiError } from '../../../api/client';
import type { ClusterConfig } from '../../../api/types';
import { CheckOption } from '../../../ds';
import { Icon } from '../../../components/Icon';
import { DurationField, NumberField, SelectField } from '../../../components/FormFields';
import type { SectionProps } from './types';

/** Applies the same key rename the server does, to every place a drive name
 *  appears in a `ClusterConfig`, so the draft stays in sync with a
 *  `renameDrive` call without wiping any other unrelated staged edit. */
function renameDriveInDraft(draft: ClusterConfig, oldName: string, newName: string): ClusterConfig {
  const drives = { ...draft.drives };
  const drive = drives[oldName];
  delete drives[oldName];
  drives[newName] = drive;

  const nodes = Object.fromEntries(
    Object.entries(draft.nodes).map(([nodeName, node]) => {
      if (!(oldName in node.drives)) return [nodeName, node];
      const nodeDrives = { ...node.drives };
      const path = nodeDrives[oldName];
      delete nodeDrives[oldName];
      nodeDrives[newName] = path;
      return [nodeName, { ...node, drives: nodeDrives }];
    }),
  );

  return { ...draft, drives, nodes };
}

export function DrivesSection({ draft, setDraft, disabled, original }: SectionProps) {
  const entries = Object.entries(draft.drives);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState('');
  const [renameError, setRenameError] = useState<string | null>(null);
  const [renameBusy, setRenameBusy] = useState(false);

  const update = (name: string, patch: Partial<ClusterConfig['drives'][string]>) => {
    setDraft({
      ...draft,
      drives: { ...draft.drives, [name]: { ...draft.drives[name], ...patch } },
    });
  };

  const add = () => {
    let name = 'drive';
    let i = 1;
    while (draft.drives[name]) name = `drive${i++}`;
    setDraft({
      ...draft,
      drives: {
        ...draft.drives,
        [name]: { sync_interval: '15m', mode: 'replicated', served_by: [] },
      },
    });
  };

  const remove = (name: string) => {
    const next = { ...draft.drives };
    delete next[name];
    setDraft({ ...draft, drives: next });
  };

  const startRename = (name: string) => {
    setRenaming(name);
    setRenameValue(name);
    setRenameError(null);
  };

  const submitRename = async (oldName: string) => {
    const newName = renameValue.trim();
    if (!newName || newName === oldName) {
      setRenaming(null);
      return;
    }
    if (draft.drives[newName]) {
      setRenameError(`a drive named ${newName} already exists`);
      return;
    }
    // A drive only "Add drive" staged, never applied, exists nowhere the
    // server knows to rename: renaming it is just a local key swap on the
    // draft, the same as `add` picking the name in the first place.
    if (!(oldName in original.drives)) {
      setDraft(renameDriveInDraft(draft, oldName, newName));
      setRenaming(null);
      return;
    }
    setRenameBusy(true);
    setRenameError(null);
    try {
      await api.renameDrive(oldName, newName);
      setDraft(renameDriveInDraft(draft, oldName, newName));
      setRenaming(null);
    } catch (e) {
      setRenameError(e instanceof ApiError ? e.message : 'rename failed');
    } finally {
      setRenameBusy(false);
    }
  };

  return (
    <>
      {entries.map(([name, drive]) => (
        <div className="card section" key={name}>
          <div className="section-head">
            {renaming === name ? (
              <div className="flex-row" style={{ gap: 8 }}>
                <input
                  className="mono"
                  value={renameValue}
                  onChange={(e) => setRenameValue(e.target.value)}
                  disabled={renameBusy}
                  aria-label={`rename drive ${name}`}
                />
                <button onClick={() => submitRename(name)} disabled={renameBusy}>
                  <Icon name="check" /> Save
                </button>
                <button onClick={() => setRenaming(null)} disabled={renameBusy}>
                  Cancel
                </button>
              </div>
            ) : (
              <h3 className="flex-row">
                <Icon name="drives" /> {name}
                {!disabled && (
                  <button className="ghost" onClick={() => startRename(name)} aria-label={`rename ${name}`}>
                    <Icon name="edit" />
                  </button>
                )}
              </h3>
            )}
            {!disabled && (
              <button className="danger" onClick={() => remove(name)}>
                <Icon name="trash" /> Remove
              </button>
            )}
          </div>
          {renaming === name && renameError && (
            <p className="field-error">
              <Icon name="alert-circle" /> {renameError}
            </p>
          )}
          <SelectField
            id={`drive-${name}-mode`}
            label="Storage mode"
            value={drive.mode}
            onChange={(mode) => update(name, { mode: mode as 'replicated' | 'served' })}
            options={[
              { value: 'replicated', label: 'Replicated' },
              { value: 'served', label: 'Served' },
            ]}
            hint="Copy everywhere, or serve from one node."
            help="Replicated keeps a synced copy on every node that implements this drive, resynced on the interval below. Served keeps one active copy on a single node, with the rest as standby backups (pick the candidate order below)."
            configKey={`drives.${name}.mode`}
            disabled={disabled}
          />
          {drive.mode === 'replicated' ? (
            <DurationField
              id={`drive-${name}-interval`}
              label="Resync every"
              value={drive.sync_interval}
              onChange={(sync_interval) => update(name, { sync_interval })}
              hint="e.g. 15m."
              help="How often each replica resyncs against the primary copy, e.g. 15m. A stack can override this per volume."
              configKey={`drives.${name}.sync_interval`}
              disabled={disabled}
            />
          ) : (
            <>
              <ServedByField
                id={`drive-${name}-servedby`}
                servedBy={drive.served_by}
                nodeNames={Object.keys(draft.nodes)}
                onChange={(served_by) => update(name, { served_by })}
                disabled={disabled}
              />
              <SelectField
                id={`drive-${name}-failback`}
                label="Fail back"
                value={drive.failback ?? 'auto'}
                onChange={(failback) => update(name, { failback: failback === 'manual' ? 'manual' : undefined })}
                options={[
                  { value: 'auto', label: 'Auto' },
                  { value: 'manual', label: 'Manual' },
                ]}
                hint="Return to the preferred node."
                help="Auto hands the drive back to the first node in Served by once it is back and has caught up. Manual keeps serving from the standby until you ask."
                configKey={`drives.${name}.failback`}
                disabled={disabled}
              />
            </>
          )}
          <NumberField
            id={`drive-${name}-parallel`}
            label="Parallel syncs"
            value={drive.max_parallel_syncs ?? 1}
            onChange={(n) => update(name, { max_parallel_syncs: Number.isInteger(n) && n > 1 ? n : undefined })}
            hint="Volumes of this drive replicated at once."
            help="When several volumes of this drive are due together, this many are replicated at the same time from a node; the rest wait their turn, most important stack first. 1 (the default) syncs them one by one."
            configKey={`drives.${name}.max_parallel_syncs`}
            disabled={disabled}
          />
        </div>
      ))}
      {!disabled && (
        <button onClick={add}>
          <Icon name="plus" /> Add drive
        </button>
      )}
    </>
  );
}

/** Picks the ordered list of candidate serving nodes for a served drive. The
 *  first checked node, in the order checked, is the active server; the rest
 *  are standby backups in priority order. Reorder with the arrows. */
function ServedByField({
  id,
  servedBy,
  nodeNames,
  onChange,
  disabled,
}: {
  id: string;
  servedBy: string[];
  nodeNames: string[];
  onChange: (servedBy: string[]) => void;
  disabled?: boolean;
}) {
  const toggle = (node: string) => {
    if (servedBy.includes(node)) onChange(servedBy.filter((n) => n !== node));
    else onChange([...servedBy, node]);
  };
  const move = (index: number, delta: number) => {
    const target = index + delta;
    if (target < 0 || target >= servedBy.length) return;
    const next = [...servedBy];
    [next[index], next[target]] = [next[target], next[index]];
    onChange(next);
  };

  return (
    <div className="field">
      <label id={`${id}-label`}>Served by</label>
      <span className="field-hint">
        The active server is the first reachable candidate; the rest keep a synced backup copy.
      </span>
      {nodeNames.map((node) => (
        <CheckOption key={node} checked={servedBy.includes(node)} onChange={() => toggle(node)} disabled={disabled}>
          {node}
        </CheckOption>
      ))}
      {servedBy.length > 0 && (
        <ol style={{ marginTop: 8 }}>
          {servedBy.map((node, i) => (
            <li key={node} className="flex-row">
              {node}
              {!disabled && (
                <>
                  <button
                    type="button"
                    className="ghost"
                    onClick={() => move(i, -1)}
                    disabled={i === 0}
                    aria-label={`move ${node} up`}
                  >
                    &#9650;
                  </button>
                  <button
                    type="button"
                    className="ghost"
                    onClick={() => move(i, 1)}
                    disabled={i === servedBy.length - 1}
                    aria-label={`move ${node} down`}
                  >
                    &#9660;
                  </button>
                </>
              )}
            </li>
          ))}
        </ol>
      )}
    </div>
  );
}
