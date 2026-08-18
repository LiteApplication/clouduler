import { useState } from 'react';
import { api, ApiError } from '../api/client';
import { Icon } from '../components/Icon';

// Minimal add-node form: name, role and rpc address. Network/drive wiring for
// a new node happens through the Configuration view, which owns the full
// cluster config shape; this just gets the node registered.
export function AddNodeForm({ onDone }: { onDone: () => void }) {
  const [name, setName] = useState('');
  const [role, setRole] = useState<'manager' | 'worker'>('worker');
  const [rpcAddr, setRpcAddr] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      await api.addNode({ name, role, rpc_addr: rpcAddr, drives: {}, availability: 'active' });
      onDone();
    } catch (err) {
      setError(err instanceof ApiError ? err.message : 'failed to add node');
    } finally {
      setBusy(false);
    }
  };

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
