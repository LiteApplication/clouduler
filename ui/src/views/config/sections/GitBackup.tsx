import { generateX25519Identity, identityToRecipient } from 'age-encryption';
import { useEffect, useState } from 'react';
import { api, ApiError } from '../../../api/client';
import type { ClusterConfig, GitBackupStatus, GitBackupWebhookSecretStatus } from '../../../api/types';
import { Tag } from '../../../ds';
import { Icon } from '../../../components/Icon';
import { CheckboxField, TextField } from '../../../components/FormFields';
import type { SectionProps } from './types';

const DEFAULT_GIT_BACKUP: NonNullable<ClusterConfig['backup']> = {
  repo_url: '',
  age_recipient: '',
  branch: 'main',
};

/** Generates a fresh age (X25519) keypair entirely client-side, downloads the
 *  private half as a file, and hands back the public recipient string to
 *  autofill the field above. The private key never touches the server -
 *  clouduler only ever needs the recipient to encrypt secrets for. */
async function generateAndDownloadAgeKey(): Promise<string> {
  const identity = await generateX25519Identity();
  const recipient = await identityToRecipient(identity);

  const contents = `# created: ${new Date().toISOString()}\n# public key: ${recipient}\n${identity}\n`;
  const blob = new Blob([contents], { type: 'text/plain' });
  const url = URL.createObjectURL(blob);
  try {
    const a = document.createElement('a');
    a.href = url;
    a.download = 'clouduler-age-key.txt';
    document.body.appendChild(a);
    a.click();
    a.remove();
  } finally {
    URL.revokeObjectURL(url);
  }

  return recipient;
}

export function GitBackupSection({ draft, setDraft, disabled }: SectionProps) {
  const b = draft.backup;
  const set = (patch: Partial<NonNullable<ClusterConfig['backup']>>) =>
    setDraft({
      ...draft,
      backup: b ? { ...b, ...patch } : { ...DEFAULT_GIT_BACKUP, ...patch },
    });
  const [generating, setGenerating] = useState(false);
  const [generateError, setGenerateError] = useState<string | null>(null);

  const generateKey = async () => {
    setGenerateError(null);
    setGenerating(true);
    try {
      const age_recipient = await generateAndDownloadAgeKey();
      set({ age_recipient });
    } catch (err) {
      setGenerateError(err instanceof Error ? err.message : 'failed to generate age key');
    } finally {
      setGenerating(false);
    }
  };

  return (
    <div className="card">
      <CheckboxField
        id="backup-enabled"
        label="Back up cluster state to git"
        value={b != null}
        onChange={(enabled) => setDraft({ ...draft, backup: enabled ? DEFAULT_GIT_BACKUP : null })}
        hint="Pushed whenever something changes."
        help="The leader pushes cluster.yaml, stack specs, configs and secrets to this repo as soon as they change, so it can be rebuilt from git after a total loss. Secrets are re-encrypted for the age key below - the cluster's own key never leaves it."
        configKey="backup"
        disabled={disabled}
      />
      {b && (
        <>
          <TextField
            id="backup-repo-url"
            label="Repository URL"
            value={b.repo_url}
            onChange={(repo_url) => set({ repo_url })}
            hint="SSH, e.g. git@github.com:org/repo.git."
            help="An SSH url, e.g. git@github.com:org/repo.git. Push auth uses a deploy key clouduler generates itself - add its public half as a write-capable deploy key on the repo once it appears below."
            configKey="backup.repo_url"
            disabled={disabled}
          />
          <TextField
            id="backup-age-recipient"
            label="Encrypt secrets for"
            value={b.age_recipient}
            onChange={(age_recipient) => set({ age_recipient })}
            hint="An age public key, age1..."
            help="The admin's age (X25519) public key, e.g. age1.... Secrets are encrypted for this key before being committed; only whoever holds the matching private key, generated and kept outside clouduler entirely, can read them back."
            configKey="backup.age_recipient"
            disabled={disabled}
          />
          <div className="btn-row" style={{ marginTop: -8, marginBottom: 16 }}>
            <button type="button" onClick={generateKey} disabled={disabled || generating}>
              <Icon name="download" /> {generating ? 'Generating...' : 'Generate age key'}
            </button>
            <span className="field-hint">
              Generates a keypair in your browser and downloads the private key - it is never sent to the server. The
              public recipient above is filled in automatically; store the downloaded file somewhere safe, it is the
              only copy.
            </span>
          </div>
          {generateError && (
            <p className="field-error">
              <Icon name="alert-circle" /> {generateError}
            </p>
          )}
          <TextField
            id="backup-branch"
            label="Branch"
            configKey="backup.branch"
            value={b.branch}
            onChange={(branch) => set({ branch })}
            disabled={disabled}
          />
          <GitBackupDeployKey />
        </>
      )}
    </div>
  );
}

/** Shows the leader-generated deploy key, last export/pull status, and the
 *  webhook secret setup, polled once on mount - this is read-only reporting
 *  (plus the write-only webhook token), not part of the draft, so it does
 *  not need to track edits like the fields above. */
function GitBackupDeployKey() {
  const [status, setStatus] = useState<GitBackupStatus | null>(null);
  const [copied, setCopied] = useState(false);
  const [webhookSecret, setWebhookSecret] = useState<GitBackupWebhookSecretStatus | null>(null);
  const [tokenInput, setTokenInput] = useState('');
  const [busy, setBusy] = useState(false);
  const [tokenError, setTokenError] = useState<string | null>(null);

  useEffect(() => {
    api
      .gitBackupStatus()
      .then(setStatus)
      .catch(() => setStatus(null));
    api
      .gitBackupWebhookSecret()
      .then(setWebhookSecret)
      .catch(() => setWebhookSecret(null));
  }, []);

  const copy = async () => {
    if (!status?.deploy_public_key) return;
    await navigator.clipboard.writeText(status.deploy_public_key);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };

  const saveWebhookSecret = async () => {
    if (!tokenInput.trim()) return;
    setTokenError(null);
    setBusy(true);
    try {
      await api.setGitBackupWebhookSecret(tokenInput.trim());
      setTokenInput('');
      setWebhookSecret(await api.gitBackupWebhookSecret());
    } catch (e) {
      setTokenError(e instanceof ApiError ? e.message : 'failed to save the webhook token');
    } finally {
      setBusy(false);
    }
  };

  if (!status) {
    return (
      <p className="loading">
        <span className="spinner" /> Checking the deploy key...
      </p>
    );
  }

  return (
    <div className="field">
      <label>Deploy key</label>
      {status.deploy_public_key ? (
        <>
          <span className="field-hint">
            Add this as a write-capable deploy key on the repository above; the leader generated it and holds the
            private half.
          </span>
          <pre className="mono">{status.deploy_public_key}</pre>
          <div className="btn-row" style={{ marginTop: 8 }}>
            <button type="button" onClick={copy}>
              <Icon name={copied ? 'check' : 'copy'} /> {copied ? 'Copied' : 'Copy public key'}
            </button>
          </div>
        </>
      ) : (
        <span className="field-hint">
          Not generated yet - the leader creates one on its first export tick after saving a repository url above.
        </span>
      )}
      {status.last_export && (
        <p className={status.last_export.error ? 'field-error' : 'muted'} style={{ marginTop: 8 }}>
          {status.last_export.error ? (
            <>
              <Icon name="alert-circle" /> Last export failed: {status.last_export.error}
            </>
          ) : (
            <>Last export: {status.last_export.summary}</>
          )}
        </p>
      )}

      <label style={{ marginTop: 16 }}>Webhook (git &rarr; cluster)</label>
      <span className="field-hint">
        Point the repository's webhook at <code>/api/git-backup/webhook/&lt;token&gt;</code>, with the token below
        pasted directly into the url - most webhook integrations offer no way to attach a custom header, but every one
        of them accepts an arbitrary url, so the token lives there instead. Treat the full url as the secret: it grants
        the same access a header token would have. A push notification pulls the branch above; a commit that fails to
        parse or validate is never applied - it is preserved on its own
        <code> rejected/...</code> branch and the branch above is force-pushed back to a tree reflecting what the
        cluster is currently running. Use <code>clouduler git-backup pull</code> to trigger the same pull by hand.
      </span>
      <input
        id="backup-webhook-token"
        type="password"
        autoComplete="new-password"
        placeholder={webhookSecret?.configured ? 'already configured' : 'token to paste into the webhook url'}
        value={tokenInput}
        onChange={(e) => setTokenInput(e.target.value)}
        disabled={busy}
        style={{ marginTop: 8 }}
      />
      <div className="btn-row" style={{ marginTop: 8 }}>
        <button type="button" onClick={saveWebhookSecret} disabled={busy || !tokenInput.trim()}>
          <Icon name="key" /> Save webhook token
        </button>
        {webhookSecret?.configured && <Tag tone="ok">configured</Tag>}
      </div>
      {tokenError && (
        <p className="field-error" style={{ marginTop: 8 }}>
          <Icon name="alert-circle" /> {tokenError}
        </p>
      )}
      {status.last_pull && (
        <p className={status.last_pull.rejected ? 'field-error' : 'muted'} style={{ marginTop: 8 }}>
          {status.last_pull.rejected ? (
            <>
              <Icon name="alert-circle" /> Last pull rejected: {status.last_pull.rejected.reason} (preserved on{' '}
              <code>{status.last_pull.rejected.preserved_branch}</code>)
            </>
          ) : (
            <>Last pull applied commit {status.last_pull.commit}</>
          )}
        </p>
      )}
    </div>
  );
}
