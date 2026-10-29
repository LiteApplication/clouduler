import { useEffect, useState } from 'react';
import { api, ApiError, encodeText } from '../../api/client';
import type { StackRequirements } from '../../api/types';
import { Button, Chip, Icon, Panel, PlacementTableFor, StatusBadge, TextField } from '../../ds';
import { useAuth } from '../../auth/AuthContext';
import { useClusterState } from '../../state/store';
import { SecretForm } from '../SecretForm';
import { usePlacementContext } from './specTools';
import './workloads.css';

/** Why each node can or cannot run this stack, one service at a time, with a
 *  way to create a missing config or secret without leaving the page. */
export function PlacementPanel({ perService }: { perService: Record<string, StackRequirements> }) {
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const ctx = usePlacementContext();
  const names = Object.keys(perService);
  const [service, setService] = useState<string | null>(names[0] ?? null);
  const [creatingConfig, setCreatingConfig] = useState<string | null>(null);
  const [creatingSecret, setCreatingSecret] = useState<string | null>(null);

  // Keep the chosen service while the spec is edited, fall back when it goes.
  const key = names.join('\n');
  useEffect(() => {
    setService((prev) => (prev && names.includes(prev) ? prev : (names[0] ?? null)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  if (!cluster) return null;
  if (names.length === 0) {
    return <p className="sub">No services in this stack yet, so there is nothing to place.</p>;
  }
  const current = service && perService[service] ? service : names[0];

  return (
    <div className="sec">
      {names.length > 1 && (
        <div className="chips" role="group" aria-label="Service">
          {names.map((n) => (
            <Chip key={n} label={n} selected={n === current} onClick={() => setService(n)} />
          ))}
        </div>
      )}
      <PlacementTableFor
        requirements={perService[current]}
        cluster={cluster}
        context={ctx}
        action={(row) =>
          isAdmin && row.missing && row.name ? (
            <Button
              size="sm"
              icon="plus"
              onClick={() => (row.kind === 'secret' ? setCreatingSecret(row.name!) : setCreatingConfig(row.name!))}
            >
              {`Create ${row.kind} ${row.name}`}
            </Button>
          ) : null
        }
      />
      {isAdmin && creatingConfig && <ConfigQuickCreate name={creatingConfig} onDone={() => setCreatingConfig(null)} />}
      {isAdmin && creatingSecret && (
        <Panel title={`Create secret ${creatingSecret}`}>
          <SecretForm initialName={creatingSecret} lockName onDone={() => setCreatingSecret(null)} />
        </Panel>
      )}
    </div>
  );
}

function ConfigQuickCreate({ name, onDone }: { name: string; onDone: () => void }) {
  const [content, setContent] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.putConfig(name, encodeText(content));
      onDone();
    } catch (e) {
      console.error(`[config] failed to save "${name}":`, e);
      setError(e instanceof ApiError ? e.message : 'The config could not be saved. Try again.');
    } finally {
      setBusy(false);
    }
  };

  return (
    <Panel title={`Create config ${name}`}>
      <input type="file" aria-label="Load content from a file" onChange={(e) => e.target.files?.[0]?.text().then(setContent)} />
      <TextField label="Content" multiline mono rows={8} value={content} error={error ?? undefined} onChange={(e) => setContent(e.target.value)} />
      <div className="acts">
        <Button variant="primary" icon="check" onClick={save} loading={busy}>
          {`Save ${name}`}
        </Button>
        <Button variant="ghost" onClick={onDone} disabled={busy}>
          Cancel
        </Button>
      </div>
    </Panel>
  );
}

/** DNS and certificate state for a stack's proxied hostnames. */
export function DnsCertPanel({ hosts }: { hosts: string[] }) {
  const cluster = useClusterState();
  if (!cluster || hosts.length === 0) return null;
  const liveDns = new Set(Object.values(cluster.dns_records).map((r) => r.fqdn));

  const state = (live: boolean, status: (typeof cluster.dns_status)[string] | undefined, liveLabel: string) => {
    if (live) return <StatusBadge status="healthy" label={liveLabel} />;
    if (status === 'Pending') return <StatusBadge status="updating" label="Pending" />;
    if (status) return <StatusBadge status="failed" label="Error" />;
    return <StatusBadge status="stopped" label="Not started" />;
  };

  return (
    <Panel title="DNS and certificates" flush>
      <div className="list list--bare">
        {hosts.map((host) => {
          const dnsErr = cluster.dns_status[host];
          const certErr = cluster.cert_status[host];
          const reason = [dnsErr, certErr].map((s) => (s && s !== 'Pending' ? s.Error.message : null)).find(Boolean);
          return (
            <div className="li" key={host}>
              <div className="li__main">
                <span className="li__name mono">{host}</span>
                {reason && (
                  <span className="li__meta wl-danger">
                    <Icon name="x" size={14} /> {reason}
                  </span>
                )}
              </div>
              <div className="li__side">
                <span className="sub">DNS</span>
                {state(liveDns.has(host), dnsErr, 'Live')}
                <span className="sub">Certificate</span>
                {state(!!cluster.certificates[host], certErr, 'Issued')}
              </div>
            </div>
          );
        })}
      </div>
    </Panel>
  );
}
