//! Certificate and DNS reconcile handlers.

use super::*;

impl ClusterState {
    pub(super) fn apply_reconcile(&mut self, req: &Request) -> Result<(), String> {
        match req {
            Request::StoreCertificate(cert) => {
                self.cert_status.remove(&cert.fqdn);
                self.certificates.insert(cert.fqdn.clone(), cert.clone());
                Ok(())
            }

            Request::RemoveCertificate { fqdn } => {
                self.certificates.remove(fqdn);
                Ok(())
            }

            Request::SetCertStatus { fqdn, status } => {
                self.cert_status.insert(fqdn.clone(), status.clone());
                Ok(())
            }

            Request::UpsertDnsRecord(record) => {
                self.dns_status.remove(&record.fqdn);
                let key = dns_key(&record.fqdn, &record.record_type, &record.content);
                self.dns_records.insert(key, record.clone());
                Ok(())
            }

            Request::RemoveDnsRecord { key } => {
                self.dns_records.remove(key);
                Ok(())
            }

            Request::SetDnsStatus { fqdn, status } => {
                self.dns_status.insert(fqdn.clone(), status.clone());
                Ok(())
            }

            Request::ClearReconcileStatus { fqdn } => {
                self.cert_status.remove(fqdn);
                self.dns_status.remove(fqdn);
                Ok(())
            }

            _ => unreachable!("apply_reconcile was handed a request routed elsewhere"),
        }
    }
}
