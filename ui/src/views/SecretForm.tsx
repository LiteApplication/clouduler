import { useState } from 'react';
import { api, ApiError, encodeText } from '../api/client';
import type { SecretCharset, SecretPutRequest, SecretSummary } from '../api/types';
import { Banner, Tag } from '../ds';
import { previewFor, validateGeneratorLength } from '../lib/secretGenerator';
import { Icon } from '../components/Icon';

type Mode = 'paste' | 'file' | 'generate';

export function SecretForm({
  onDone,
  initialName = '',
  lockName = false,
  rotate,
}: {
  onDone: () => void;
  /** Prefills the name, e.g. when creating a secret a stack is already
   *  missing rather than starting from a blank form. */
  initialName?: string;
  /** Locks the name field, so the created secret is guaranteed to match what
   *  the caller is waiting on. */
  lockName?: boolean;
  /** Rotate this existing secret instead of creating one: the name is fixed,
   *  its users are listed, and the form offers to restart them. */
  rotate?: SecretSummary;
}) {
  const [name, setName] = useState(rotate?.name ?? initialName);
  const [restart, setRestart] = useState(false);
  const [mode, setMode] = useState<Mode>('paste');
  const [value, setValue] = useState('');
  const [fileContent, setFileContent] = useState('');
  const [length, setLength] = useState(32);
  const [charset, setCharset] = useState<SecretCharset>('alphanumeric');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const lengthCheck = validateGeneratorLength(length);

  const onFile = async (file: File) => {
    setFileContent(await file.text());
  };

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    if (!name.trim()) {
      setError('name is required');
      return;
    }
    if (mode === 'generate' && !lengthCheck.valid) {
      setError(lengthCheck.error);
      return;
    }
    setBusy(true);
    try {
      const body: SecretPutRequest =
        mode === 'paste'
          ? { value }
          : mode === 'file'
            ? { content: encodeText(fileContent) }
            : { generate: { length, charset } };
      if (rotate) {
        await api.rotateSecret(name.trim(), { ...body, restart });
      } else {
        await api.putSecret(name.trim(), body);
      }
      onDone();
    } catch (err) {
      console.error(`[secret] failed to save "${name}":`, err);
      setError(err instanceof ApiError ? err.message : 'failed to save secret');
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="cfg">
      <form onSubmit={submit} className="card section">
        <h3 className="flex-row">
          <Icon name="key" /> {rotate ? `Rotate ${rotate.name}` : 'New secret'}
        </h3>
        {rotate && <RotateNotes secret={rotate} restart={restart} onRestart={setRestart} />}
        <div className="field">
          <label htmlFor="secret-name">Name</label>
          <input
            id="secret-name"
            type="text"
            value={name}
            onChange={(e) => setName(e.target.value)}
            disabled={lockName || !!rotate}
            required
          />
        </div>

        <div className="field">
          <label>Source</label>
          <div className="segmented" role="tablist">
            <button type="button" role="tab" aria-selected={mode === 'paste'} onClick={() => setMode('paste')}>
              <Icon name="edit" /> Paste string
            </button>
            <button type="button" role="tab" aria-selected={mode === 'file'} onClick={() => setMode('file')}>
              <Icon name="file" /> File
            </button>
            <button type="button" role="tab" aria-selected={mode === 'generate'} onClick={() => setMode('generate')}>
              <Icon name="dice" /> Generate
            </button>
          </div>
        </div>

        {mode === 'paste' && (
          <div className="field">
            <label htmlFor="secret-value">Value</label>
            <input
              id="secret-value"
              type="password"
              value={value}
              onChange={(e) => setValue(e.target.value)}
              autoComplete="new-password"
            />
          </div>
        )}

        {mode === 'file' && (
          <div className="field">
            <label htmlFor="secret-file">File</label>
            <input
              id="secret-file"
              type="file"
              onChange={(e) => e.target.files?.[0] && onFile(e.target.files[0])}
            />
            <textarea
              aria-label="File contents"
              rows={8}
              value={fileContent}
              onChange={(e) => setFileContent(e.target.value)}
              placeholder="paste or edit file contents here"
            />
          </div>
        )}

        {mode === 'generate' && (
          <>
            <div className="field">
              <label htmlFor="secret-length">Length</label>
              <input
                id="secret-length"
                type="number"
                value={length}
                onChange={(e) => setLength(Number(e.target.value))}
                className={lengthCheck.valid ? '' : 'invalid'}
              />
              {!lengthCheck.valid && <p className="field-error">{lengthCheck.error}</p>}
            </div>
            <div className="field">
              <label htmlFor="secret-charset">Character set</label>
              <select id="secret-charset" value={charset} onChange={(e) => setCharset(e.target.value as SecretCharset)}>
                <option value="alphanumeric">Alphanumeric</option>
                <option value="hex">Hex</option>
                <option value="base64">Base64</option>
                <option value="printable">Printable</option>
              </select>
            </div>
            <div className="field">
              <label>Preview of the format (not the actual value)</label>
              <p className="mono muted">{previewFor(charset, length)}</p>
            </div>
          </>
        )}

        {error && (
          <p className="field-error">
            <Icon name="alert-circle" /> {error}
          </p>
        )}
        <p className="field-hint">
          The value is sealed on the server and never returned by the API, so this is the only chance to copy it down
          if you set it yourself.
        </p>
        <div className="btn-row">
          <button type="submit" className="primary" disabled={busy}>
            <Icon name="key" /> {rotate ? (restart ? 'Rotate and restart now' : 'Rotate, store only') : 'Create secret'}
          </button>
          <button type="button" onClick={onDone} disabled={busy}>
            <Icon name="close" /> Cancel
          </button>
        </div>
      </form>
    </div>
  );
}

/** What rotating does and does not do, who is affected, and the restart choice. */
function RotateNotes({
  secret,
  restart,
  onRestart,
}: {
  secret: SecretSummary;
  restart: boolean;
  onRestart: (on: boolean) => void;
}) {
  const users = secret.used_by;
  return (
    <>
      <Banner tone="warn" title="Clouduler does not migrate data derived from this secret">
        A database password stored inside the database, or an encryption key protecting data at rest, keeps its old
        value there. Rotating those needs the application's own procedure: change it in the app first or together
        with this, otherwise the service can lock itself out.
      </Banner>
      <div className="field">
        <label>Used by</label>
        {users.length === 0 ? (
          <p className="muted">No running instance references this secret.</p>
        ) : (
          <ul style={{ listStyle: "none", padding: 0, margin: 0 }}>
            {users.map((u) => (
              <li key={`${u.instance}:${u.target}`} className="flex-row">
                <code>{u.instance}</code>
                <span className="muted">
                  {u.delivery === 'env' ? `env ${u.target}` : `file ${u.target}`}
                </span>
                {u.freshness === 'stale' && <Tag tone="warn">stale: runs r{u.running_revision}</Tag>}
                {u.freshness === 'current' && <Tag tone="ok">current</Tag>}
                {u.freshness === 'unknown' && <Tag>revision unknown</Tag>}
              </li>
            ))}
          </ul>
        )}
        <p className="field-hint">
          Current revision is r{secret.revision}. Containers keep the value they started with, for files and env
          variables alike, until they are recreated.
        </p>
      </div>
      <div className="field">
        <label>After storing the new value</label>
        <label className="flex-row">
          <input type="radio" name="rotate-restart" checked={!restart} onChange={() => onRestart(false)} /> Store
          only, do not restart (instances show as stale)
        </label>
        <label className="flex-row">
          <input type="radio" name="rotate-restart" checked={restart} onChange={() => onRestart(true)} /> Rotate and
          restart now: one instance at a time per service, waiting for health, stopping at the first failure
        </label>
      </div>
    </>
  );
}
