use super::*;

#[test]
fn storing_a_certificate_clears_its_pending_status() {
    let mut state = ClusterState::default();
    state.apply(&Request::SetCertStatus {
        fqdn: "app.example.com".into(),
        status: ReconcileStatus::Pending,
    });
    assert_eq!(
        state.cert_status.get("app.example.com"),
        Some(&ReconcileStatus::Pending)
    );

    state.apply(&Request::StoreCertificate(CertificateState {
        fqdn: "app.example.com".into(),
        chain_pem: "chain".into(),
        key_pem: "key".into(),
        not_after: 0,
        issued_at: 0,
    }));

    assert!(state.certificates.contains_key("app.example.com"));
    assert!(!state.cert_status.contains_key("app.example.com"));
}

#[test]
fn clearing_reconcile_status_drops_cert_and_dns_entries() {
    let mut state = ClusterState::default();
    let err = ReconcileStatus::Error {
        message: "no zone".into(),
        at: 0,
    };
    state.apply(&Request::SetCertStatus {
        fqdn: "typo.example.com".into(),
        status: err.clone(),
    });
    state.apply(&Request::SetDnsStatus {
        fqdn: "typo.example.com".into(),
        status: err,
    });
    state.apply(&Request::ClearReconcileStatus {
        fqdn: "typo.example.com".into(),
    });
    assert!(state.cert_status.is_empty());
    assert!(state.dns_status.is_empty());
}

#[test]
fn upserting_a_dns_record_clears_its_status_for_that_fqdn() {
    let mut state = ClusterState::default();
    state.apply(&Request::SetDnsStatus {
        fqdn: "app.example.com".into(),
        status: ReconcileStatus::Error {
            message: "no zone".into(),
            at: 0,
        },
    });
    assert!(state.dns_status.contains_key("app.example.com"));

    state.apply(&Request::UpsertDnsRecord(DnsRecordState {
        fqdn: "app.example.com".into(),
        record_type: "A".into(),
        content: "10.0.0.1".into(),
        zone: "example.com".into(),
        provider_id: Some("rec1".into()),
    }));

    assert!(state
        .dns_records
        .values()
        .any(|r| r.fqdn == "app.example.com"));
    assert!(!state.dns_status.contains_key("app.example.com"));
}
