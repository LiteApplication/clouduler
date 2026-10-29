// /dns: certificates and the DNS records Clouduler keeps in sync, with the
// TLS fingerprint a joining node pins.

import { useEffect, useState } from 'react';
import { api, ApiError } from '../../api/client';
import type { ReconcileStatus, TlsFingerprint } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Banner, Button, PageHeader, Panel, Tag, useConfirm, useToast } from '../../ds';
import { daysUntil, relativeTime } from '../../lib/format';
import { useClusterState } from '../../state/store';
import { DataTable } from './table';

/** Pending / error tag for a hostname with nothing issued or published yet. */
function StatusTag({ status }: { status: ReconcileStatus | undefined }) {
  if (!status) return null;
  if (status === 'Pending') return <Tag tone="warn">pending</Tag>;
  return <Tag tone="danger">error: {status.Error.message}</Tag>;
}

export function Dns() {
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const confirm = useConfirm();
  const toast = useToast();
  const [busy, setBusy] = useState<string | null>(null);
  const [reconciling, setReconciling] = useState(false);
  const [tls, setTls] = useState<TlsFingerprint | null>(null);

  useEffect(() => {
    api.tlsFingerprint().then(setTls).catch(() => {});
  }, []);

  if (!cluster) return <p className="cl-muted">Loading…</p>;

  const certs = Object.values(cluster.certificates).sort((a, b) => a.not_after - b.not_after);
  const records = Object.values(cluster.dns_records);
  const certStatus = cluster.cert_status ?? {};
  const dnsStatus = cluster.dns_status ?? {};
  const liveHosts = new Set(records.map((r) => r.fqdn));
  const pendingDns = Object.keys(dnsStatus).filter((f) => !liveHosts.has(f));
  const failed = Object.entries(certStatus).filter(([, s]) => s !== 'Pending');
  const next = certs[0];

  const fail = (e: unknown, fallback: string) => toast(e instanceof ApiError ? e.message : fallback, { tone: 'danger' });

  const renew = async (fqdn: string) => {
    const ok = await confirm({
      title: `Renew ${fqdn}?`,
      body: 'The current certificate is removed before a new one is requested, so the host may briefly lack TLS.',
      confirmLabel: `Renew ${fqdn}`,
      confirmVariant: 'primary',
    });
    if (!ok) return;
    setBusy(fqdn);
    try {
      await api.renewCertificate(fqdn);
      toast(`Requested a new certificate for ${fqdn}.`);
    } catch (e) {
      fail(e, 'Could not renew that certificate.');
    } finally {
      setBusy(null);
    }
  };

  const reconcile = async () => {
    setReconciling(true);
    try {
      await api.reconcileDns();
      toast('Reconciling DNS now.');
    } catch (e) {
      fail(e, 'Could not reconcile DNS.');
    } finally {
      setReconciling(false);
    }
  };

  const copyFingerprint = async () => {
    if (!tls) return;
    try {
      await navigator.clipboard.writeText(tls.fingerprint);
      toast('Copied the fingerprint.');
    } catch {
      toast('Could not copy. Select the fingerprint instead.', { tone: 'warn' });
    }
  };

  const pending = Object.keys(certStatus).length + pendingDns.length;

  return (
    <>
      <PageHeader
        eyebrow="Network & data"
        title="DNS"
        sub={next ? `Next certificate expires in ${daysUntil(next.not_after)} days.` : 'Certificates and the records that point at them.'}
        actions={isAdmin && <Button variant="primary" icon="sync" loading={reconciling} onClick={() => void reconcile()}>Reconcile DNS</Button>}
      />
      <div className="stack">
        {failed.length > 0 && (
          <Banner tone="danger" title={`${failed.length === 1 ? 'A certificate' : `${failed.length} certificates`} could not be issued`}>
            {failed.map(([fqdn, s]) => `${fqdn}: ${(s as { Error: { message: string } }).Error.message}`).join('. ')}
          </Banner>
        )}

        <div className="grid grid--3">
          <Panel title="Certificates"><p className="h1">{certs.length}</p><p className="cl-sm cl-muted">{Object.keys(certStatus).length} pending or failing</p></Panel>
          <Panel title="Next expiry"><p className="h1">{next ? `${daysUntil(next.not_after)}d` : '-'}</p><p className="cl-sm cl-muted">{next?.fqdn ?? 'No certificates yet'}</p></Panel>
          <Panel title="DNS records"><p className="h1">{records.length}</p><p className="cl-sm cl-muted">{pending > 0 ? `${pending} pending` : 'All published'}</p></Panel>
        </div>

        <Panel title="Certificates" flush>
          <DataTable
            caption="Certificates"
            rows={[
              ...certs.map((c) => ({ id: c.fqdn, fqdn: c.fqdn, days: daysUntil(c.not_after), issued: c.issued_at, status: undefined as ReconcileStatus | undefined })),
              ...Object.entries(certStatus).map(([fqdn, status]) => ({ id: `s-${fqdn}`, fqdn, days: null as number | null, issued: undefined, status })),
            ]}
            empty="No certificates yet."
            columns={[
              { key: 'fqdn', header: 'Host', render: (r) => <strong>{r.fqdn}</strong> },
              {
                key: 'exp',
                header: 'Expires',
                render: (r) =>
                  r.days == null ? <StatusTag status={r.status} /> : <Tag tone={r.days < 0 || r.days < 14 ? 'danger' : r.days < 30 ? 'warn' : 'ok'}>{r.days < 0 ? 'expired' : `${r.days} days left`}</Tag>,
              },
              { key: 'issued', header: 'Issued', render: (r) => (r.issued ? relativeTime(r.issued * 1000) : '-') },
              ...(isAdmin
                ? [{ key: 'act', header: '', render: (r: { fqdn: string; days: number | null }) => r.days == null ? null : <Button size="sm" icon="sync" disabled={busy === r.fqdn} onClick={() => void renew(r.fqdn)}>Renew {r.fqdn}</Button> }]
                : []),
            ]}
          />
        </Panel>

        <Panel title="DNS records" flush>
          <DataTable
            caption="DNS records"
            rows={[
              ...records.map((r) => ({ id: `${r.fqdn}|${r.record_type}|${r.content}`, fqdn: r.fqdn, type: r.record_type, content: r.content, zone: r.zone, status: undefined as ReconcileStatus | undefined })),
              ...pendingDns.map((f) => ({ id: `p-${f}`, fqdn: f, type: '-', content: '-', zone: '-', status: dnsStatus[f] })),
            ]}
            empty="No records yet."
            columns={[
              { key: 'fqdn', header: 'Host', render: (r) => <strong>{r.fqdn}</strong> },
              { key: 'type', header: 'Type', render: (r) => r.type },
              { key: 'content', header: 'Content', mono: true, render: (r) => r.content },
              { key: 'zone', header: 'Zone', render: (r) => r.zone },
              { key: 'st', header: 'Status', render: (r) => (r.status ? <StatusTag status={r.status} /> : <Tag tone="ok">live</Tag>) },
            ]}
          />
        </Panel>

        <Panel title="TLS fingerprint" actions={tls && <Button size="sm" icon="copy" onClick={() => void copyFingerprint()}>Copy fingerprint</Button>}>
          <p className="cl-sm cl-muted">New nodes pin this when they join, so they know they reached the right cluster.</p>
          <pre className="codeblock" aria-label="TLS fingerprint">{tls?.fingerprint ?? 'Not available.'}</pre>
        </Panel>
      </div>
    </>
  );
}
