import { useState } from 'react';
import { api, ApiError } from '../api/client';
import { useClusterState } from '../state/store';
import { useAuth } from '../auth/AuthContext';
import { Badge } from '../components/Badge';
import { daysUntil, formatUnixSeconds } from '../lib/format';
import { Icon } from '../components/Icon';
import type { ReconcileStatus } from '../api/types';

/** `Pending`/`Error` badge for a hostname with nothing issued/published yet.
 *  `null` once the real record/cert lands, since presence there is itself
 *  "ready" and needs no separate badge. */
export function statusBadge(status: ReconcileStatus | undefined) {
  if (!status) return null;
  if (status === 'Pending') return <Badge tone="warn">pending</Badge>;
  return <Badge tone="bad">error: {status.Error.message}</Badge>;
}

export function Certificates() {
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const [error, setError] = useState<string | null>(null);
  const [busyFqdn, setBusyFqdn] = useState<string | null>(null);

  const certs = cluster ? Object.values(cluster.certificates) : [];
  const records = cluster ? Object.values(cluster.dns_records) : [];
  const certStatus = cluster?.cert_status ?? {};
  const dnsStatus = cluster?.dns_status ?? {};
  const liveDnsHosts = new Set(records.map((r) => r.fqdn));
  const pendingDnsHosts = Object.keys(dnsStatus).filter((fqdn) => !liveDnsHosts.has(fqdn));

  const renew = async (fqdn: string) => {
    setBusyFqdn(fqdn);
    setError(null);
    try {
      await api.renewCertificate(fqdn);
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'renew failed');
    } finally {
      setBusyFqdn(null);
    }
  };

  const [reconciling, setReconciling] = useState(false);
  const reconcile = async () => {
    setReconciling(true);
    try {
      await api.reconcileDns();
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'reconcile failed');
    } finally {
      setReconciling(false);
    }
  };

  if (!cluster) {
    return (
      <p className="loading">
        <span className="spinner" /> Loading certificates...
      </p>
    );
  }

  return (
    <div>
      <div className="page-header">
        <div className="page-title">
          <Icon name="certificate" />
          <div>
            <h1>Certificates and DNS</h1>
            <p className="page-sub">
              {certs.length} certificate{certs.length === 1 ? '' : 's'} · {records.length} DNS record
              {records.length === 1 ? '' : 's'}
              {(Object.keys(certStatus).length > 0 || pendingDnsHosts.length > 0) &&
                ` · ${Object.keys(certStatus).length + pendingDnsHosts.length} pending`}
            </p>
          </div>
        </div>
      </div>
      {error && (
        <div className="banner banner-warn">
          <Icon name="alert" /> {error}
        </div>
      )}

      <div className="section">
        <div className="section-head">
          <h2>
            <Icon name="certificate" /> Certificates
          </h2>
        </div>
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>FQDN</th>
                <th>Expires</th>
                <th>Status</th>
                {isAdmin && <th>Actions</th>}
              </tr>
            </thead>
            <tbody>
              {certs.map((c) => {
                const days = daysUntil(c.not_after);
                return (
                  <tr key={c.fqdn}>
                    <td>{c.fqdn}</td>
                    <td>{formatUnixSeconds(c.not_after)}</td>
                    <td>
                      <Badge tone={days < 14 ? 'bad' : days < 30 ? 'warn' : 'good'}>
                        {days < 0 ? 'expired' : `${days}d left`}
                      </Badge>
                    </td>
                    {isAdmin && (
                      <td>
                        <button disabled={busyFqdn === c.fqdn} onClick={() => renew(c.fqdn)}>
                          <Icon name="refresh" /> Renew
                        </button>
                      </td>
                    )}
                  </tr>
                );
              })}
              {Object.entries(certStatus).map(([fqdn, status]) => (
                <tr key={fqdn}>
                  <td>{fqdn}</td>
                  <td>—</td>
                  <td>{statusBadge(status)}</td>
                  {isAdmin && <td />}
                </tr>
              ))}
              {certs.length === 0 && Object.keys(certStatus).length === 0 && (
                <tr>
                  <td colSpan={4} className="empty-cell">
                    no certificates
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>

      <div className="section">
        <div className="section-head">
          <h2>
            <Icon name="network" /> DNS records
          </h2>
          {isAdmin && (
            <button onClick={reconcile} disabled={reconciling}>
              <Icon name="sync" /> Reconcile now
            </button>
          )}
        </div>
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>FQDN</th>
                <th>Type</th>
                <th>Content</th>
                <th>Zone</th>
                <th>Status</th>
              </tr>
            </thead>
            <tbody>
              {records.map((r) => (
                <tr key={`${r.fqdn}|${r.record_type}|${r.content}`}>
                  <td>{r.fqdn}</td>
                  <td>{r.record_type}</td>
                  <td className="mono">{r.content}</td>
                  <td>{r.zone}</td>
                  <td>
                    <Badge tone="good">live</Badge>
                  </td>
                </tr>
              ))}
              {pendingDnsHosts.map((fqdn) => (
                <tr key={fqdn}>
                  <td>{fqdn}</td>
                  <td>—</td>
                  <td>—</td>
                  <td>—</td>
                  <td>{statusBadge(dnsStatus[fqdn])}</td>
                </tr>
              ))}
              {records.length === 0 && pendingDnsHosts.length === 0 && (
                <tr>
                  <td colSpan={5} className="empty-cell">
                    no records
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
