import { useState } from 'react';
import { api, ApiError } from '../api/client';
import type { SecretCharset } from '../api/types';
import { previewFor, validateGeneratorLength } from '../lib/secretGenerator';

type Mode = 'paste' | 'file' | 'generate';

export function SecretForm({ onDone }: { onDone: () => void }) {
  const [name, setName] = useState('');
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
      if (mode === 'paste') {
        await api.putSecret(name.trim(), { value });
      } else if (mode === 'file') {
        await api.putSecret(name.trim(), { content: btoa(fileContent) });
      } else {
        await api.putSecret(name.trim(), { generate: { length, charset } });
      }
      onDone();
    } catch (err) {
      setError(err instanceof ApiError ? err.message : 'failed to save secret');
    } finally {
      setBusy(false);
    }
  };

  return (
    <form onSubmit={submit} className="card section">
      <h3>New secret</h3>
      <div className="field">
        <label htmlFor="secret-name">Name</label>
        <input id="secret-name" type="text" value={name} onChange={(e) => setName(e.target.value)} required />
      </div>

      <div className="field">
        <label>Source</label>
        <div className="btn-row">
          <button type="button" className={mode === 'paste' ? 'primary' : ''} onClick={() => setMode('paste')}>
            Paste string
          </button>
          <button type="button" className={mode === 'file' ? 'primary' : ''} onClick={() => setMode('file')}>
            File
          </button>
          <button type="button" className={mode === 'generate' ? 'primary' : ''} onClick={() => setMode('generate')}>
            Generate
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
            autoComplete="off"
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

      {error && <p className="field-error">{error}</p>}
      <p className="field-hint">
        The value is sealed on the server and never returned by the API, so this is the only chance to copy it down
        if you set it yourself.
      </p>
      <div className="btn-row">
        <button type="submit" className="primary" disabled={busy}>
          Create secret
        </button>
        <button type="button" onClick={onDone} disabled={busy}>
          Cancel
        </button>
      </div>
    </form>
  );
}
