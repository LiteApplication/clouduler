import { useState } from 'react';
import { api, ApiError } from '../api/client';
import { Icon } from '../components/Icon';

// Minimal add-node form: name, role and rpc address. Network/drive wiring for
// a new node happens through the Configuration view, which owns the full
// cluster config shape; this just gets the node registered.
//
// Once registered, the operator still has to actually run `clouduler init`
// on the physical machine. That command needs an admin api key and this
// cluster's address -- both of which the admin's already-authenticated
// browser can hand over right here, so a fresh single-use key is minted and
// shown alongside the exact command to paste. No cluster secret key ever
// needs to travel with it: a joining node gets that automatically once it
// starts and publishes its own public key (see the `cl_secret::NodeKeypair`
// / wrapped-key exchange in the backend), so there is no manual key-copy
// step left to tell the operator about.
export function AddNodeForm({ onDone }: { onDone: () => void }) {
  const [name, setName] = useState('');
  const [role, setRole] = useState<'manager' | 'worker'>('worker');
  const [rpcAddr, setRpcAddr] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [command, setCommand] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      await api.addNode({ name, role, rpc_addr: rpcAddr, drives: {}, availability: 'active' });
      const [key, config] = await Promise.all([api.createKey(`join-${name}`), api.getConfiguration()]);
      const host = window.location.hostname;
      const join = config.web.port === 443 ? host : `${host}:${config.web.port}`;
      setCommand(
        `clouduler init --yes --join ${join} --token ${key.secret} --role ${role} --node ${name}`,
      );
    } catch (err) {
      setError(err instanceof ApiError ? err.message : 'failed to add node');
    } finally {
      setBusy(false);
    }
  };

  const copy = async () => {
    if (!command) return;
    await navigator.clipboard.writeText(command);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };

  if (command) {
    return (
      <div className="card section">
        <h3 className="flex-row">
          <Icon name="terminal" /> Run this on {name}
        </h3>
        <p className="page-sub">
          The node is registered. Run this command on the new machine to have it join the
          cluster -- it carries a one-time api key that will not be shown again.
        </p>
        <pre className="mono">{command}</pre>
        <div className="btn-row">
          <button type="button" onClick={copy}>
            <Icon name={copied ? 'check' : 'copy'} /> {copied ? 'Copied' : 'Copy command'}
          </button>
          <button type="button" className="primary" onClick={onDone}>
            <Icon name="check" /> Done
          </button>
        </div>
      </div>
    );
  }

  return (
    <form onSubmit={submit} className="card section">
      <h3 className="flex-row">
        <Icon name="plus" /> Add a node
      </h3>
      <div className="field">
        <label htmlFor="node-name">Name</label>
        <input id="node-name" type="text" value={name} onChange={(e) => setName(e.target.value)} required />
      </div>
      <div className="field">
        <label htmlFor="node-role">Role</label>
        <select id="node-role" value={role} onChange={(e) => setRole(e.target.value as 'manager' | 'worker')}>
          <option value="worker">Worker</option>
          <option value="manager">Manager</option>
        </select>
      </div>
      <div className="field">
        <label htmlFor="node-rpc">RPC address (host:port)</label>
        <input id="node-rpc" type="text" value={rpcAddr} onChange={(e) => setRpcAddr(e.target.value)} required />
      </div>
      {error && (
        <p className="field-error">
          <Icon name="alert-circle" /> {error}
        </p>
      )}
      <div className="btn-row">
        <button type="submit" className="primary" disabled={busy}>
          <Icon name="plus" /> Add node
        </button>
        <button type="button" onClick={onDone}>
          <Icon name="close" /> Cancel
        </button>
      </div>
    </form>
  );
}
