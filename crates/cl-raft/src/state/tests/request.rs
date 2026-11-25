use super::*;
use crate::state::request::KNOWN_REQUEST_VARIANTS;

#[test]
fn backup_run_outcome_round_trips_the_current_shape() {
    for outcome in [
        BackupRunOutcome::Running {
            percent: Some(0.42),
        },
        BackupRunOutcome::Succeeded {
            snapshot_id: "abc".into(),
            bytes: 7,
            warning: None,
        },
        BackupRunOutcome::Failed {
            error: "boom".into(),
        },
    ] {
        let json = serde_json::to_string(&outcome).unwrap();
        let back: BackupRunOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back, outcome);
    }
}

#[test]
fn an_unrecognized_request_variant_deserializes_to_unknown_instead_of_erroring() {
    let from_a_newer_node = serde_json::json!({
        "SomeFutureVariant": { "field": "value" }
    });
    let req: Request = serde_json::from_value(from_a_newer_node.clone()).unwrap();
    assert_eq!(req, Request::Unknown(from_a_newer_node.clone()));
    // Re-serializing reproduces the payload verbatim, so an old leader
    // re-replicates it losslessly.
    assert_eq!(serde_json::to_value(&req).unwrap(), from_a_newer_node);

    let mut state = ClusterState::default();
    let before = state.version;
    let response = state.apply(&req);
    assert!(response.is_ok(), "{:?}", response.error);
    assert_eq!(state.version, before, "Unknown must not bump version");
}

#[test]
fn a_known_request_tag_with_a_bad_body_is_a_hard_error() {
    let bad = serde_json::json!({ "PutClusterConfig": { "yaml": 5 } });
    assert!(serde_json::from_value::<Request>(bad).is_err());
    // Unit variant tag used as an object, and non-tag-shaped payloads.
    assert!(serde_json::from_value::<Request>(serde_json::json!({"NoOp": {"x": 1}})).is_err());
    assert!(serde_json::from_value::<Request>(serde_json::json!(7)).is_err());
    assert!(serde_json::from_value::<Request>(serde_json::json!({"NoOp": null, "b": 1})).is_err());
}

#[test]
fn every_current_request_variant_round_trips_and_is_not_unknown() {
    // One instance per real `Request` variant (not `Unknown`); guards against
    // `request_mirror::RequestMirror` drifting, which would deserialize a variant as `Unknown`.
    let requests = vec![
        Request::PutClusterConfig {
            yaml: "y".into(),
            expect_generation: 0,
        },
        Request::UpsertNode(node("n1", &["fast"])),
        Request::SetAvailability {
            node: "n1".into(),
            availability: Availability::Drain,
        },
        Request::SetReachable {
            node: "n1".into(),
            reachable: false,
        },
        Request::RemoveNode { node: "n1".into() },
        Request::RetireNodeId {
            id: uuid::Uuid::new_v4(),
        },
        Request::DeployStack {
            name: "s".into(),
            spec: "spec".into(),
            at: 0,
        },
        Request::RemoveStack { name: "s".into() },
        Request::SetStackObjects {
            name: "s".into(),
            configs: drives(&["c1"]),
            secrets: drives(&["sec1"]),
            used_configs: drives(&["c1"]),
            used_secrets: drives(&["sec1"]),
        },
        Request::Assign {
            instance: InstanceId::new("s", "web", 0),
            node: "n1".into(),
            drives: drives(&["fast"]),
        },
        Request::Unassign {
            instance: InstanceId::new("s", "web", 0),
        },
        Request::SetDesiredState {
            instance: InstanceId::new("s", "web", 0),
            desired: DesiredState::Stopped,
        },
        Request::SetImageDigest {
            instance: InstanceId::new("s", "web", 0),
            digest: "sha256:abc".into(),
        },
        Request::BeginMigration {
            instance: InstanceId::new("s", "web", 0),
            to: "n2".into(),
            drives: drives(&["fast"]),
        },
        Request::AdvanceMigration {
            instance: InstanceId::new("s", "web", 0),
            phase: MigrationPhase::Syncing,
        },
        Request::ClearMigration {
            instance: InstanceId::new("s", "web", 0),
        },
        Request::SkipMigrationVerify {
            instance: InstanceId::new("s", "web", 0),
        },
        Request::UpsertVolume {
            id: VolumeId::new("fast", "v1"),
            primary: "n1".into(),
        },
        Request::SetVolumeReplica {
            id: VolumeId::new("fast", "v1"),
            node: "n1".into(),
            generation: 2,
        },
        Request::TouchVolume {
            id: VolumeId::new("fast", "v1"),
            primary: "n1".into(),
        },
        Request::SetVolumeSyncRefusal {
            id: VolumeId::new("fast", "v1"),
            refusal: None,
        },
        Request::SetVolumeEmpty {
            id: VolumeId::new("fast", "v1"),
            empty: true,
        },
        Request::RemoveVolume {
            id: VolumeId::new("fast", "v1"),
        },
        Request::AckVolumeRemoved {
            id: VolumeId::new("fast", "v1"),
            node: "n1".into(),
        },
        Request::FreezeVolume {
            id: VolumeId::new("fast", "v1"),
            node: "n1".into(),
            stopped: vec![],
        },
        Request::UnfreezeVolume {
            id: VolumeId::new("fast", "v1"),
        },
        Request::StoreCertificate(CertificateState {
            fqdn: "example.com".into(),
            chain_pem: "chain".into(),
            key_pem: "key".into(),
            not_after: 0,
            issued_at: 0,
        }),
        Request::RemoveCertificate {
            fqdn: "example.com".into(),
        },
        Request::SetCertStatus {
            fqdn: "example.com".into(),
            status: ReconcileStatus::Pending,
        },
        Request::UpsertDnsRecord(DnsRecordState {
            fqdn: "example.com".into(),
            record_type: "A".into(),
            content: "1.2.3.4".into(),
            zone: "example.com".into(),
            provider_id: None,
        }),
        Request::RemoveDnsRecord {
            key: "example.com".into(),
        },
        Request::SetDnsStatus {
            fqdn: "example.com".into(),
            status: ReconcileStatus::Pending,
        },
        Request::ClearReconcileStatus {
            fqdn: "example.com".into(),
        },
        Request::PutConfigCas {
            name: "c".into(),
            content: base64_encode(b"x"),
            owner: None,
            expect_revision: 1,
        },
        Request::PutConfig {
            name: "c1".into(),
            content: "aGk=".into(),
            owner: None,
        },
        Request::RemoveConfig { name: "c1".into() },
        Request::PutSecret {
            name: "sec1".into(),
            sealed: "sealed".into(),
            key_fingerprint: "fp".into(),
            owner: None,
        },
        Request::RemoveSecret {
            name: "sec1".into(),
        },
        Request::PutWrappedKey {
            node: "n1".into(),
            entry: WrappedKeyEntry {
                ephemeral_public: "e".into(),
                sealed: "s".into(),
                key_fingerprint: "fp".into(),
                target_pubkey: "t".into(),
            },
        },
        Request::SetOverlayAllocations {
            allocations: "{}".into(),
        },
        Request::PutBackupDestination(BackupDestination {
            id: "d1".into(),
            name: "d1".into(),
            kind: BackupDestinationKind::NodeLocal {
                node: "n1".into(),
                path: "/backups".into(),
            },
            password_secret: "sec1".into(),
            credentials_secret: None,
        }),
        Request::RemoveBackupDestination { id: "d1".into() },
        Request::PutBackupTarget(DriveBackupTarget {
            id: "t1".into(),
            drive: "fast".into(),
            destination_id: "d1".into(),
            enabled: true,
            frequency_secs: 3600,
            time_of_day: None,
            keep_last: None,
            keep_within_secs: None,
            keep_daily: None,
            keep_weekly: None,
            keep_monthly: None,
            keep_yearly: None,
            stop_containers: None,
            last_run: None,
        }),
        Request::RemoveBackupTarget { id: "t1".into() },
        Request::SetBackupTargetLastRun {
            id: "t1".into(),
            at: 0,
        },
        Request::RecordBackupRun(BackupRun {
            id: "r1".into(),
            target_id: "t1".into(),
            drive: "fast".into(),
            destination_id: "d1".into(),
            started_at: 0,
            finished_at: None,
            outcome: BackupRunOutcome::Running { percent: None },
            triggered_by: BackupTrigger::Manual { by: "op".into() },
        }),
        Request::PutRestoreRequest(RestoreRequest {
            id: "res1".into(),
            scope: RestoreScope::Volume {
                volume: VolumeId::new("fast", "v1"),
            },
            destination_id: None,
            snapshot: None,
            requested_at: 0,
            finished_at: None,
            status: RestoreStatus::Pending,
            requested_by: "op".into(),
        }),
        Request::SetRestoreStatus {
            id: "res1".into(),
            status: RestoreStatus::Pending,
            at: 0,
        },
        Request::ReportContainerStatus {
            node: "n1".into(),
            running: BTreeMap::from([(InstanceId::new("gitlab", "web", 0), true)]),
            secret_revisions: BTreeMap::new(),
            config_revisions: BTreeMap::new(),
        },
        Request::SetInstanceUpdate {
            instance: InstanceId::new("gitlab", "web", 0),
            node: "n1".into(),
            phase: UpdatePhase::Failed,
            error: Some("pull failed".into()),
            at: 5,
        },
        Request::ClearInstanceUpdate {
            instance: InstanceId::new("gitlab", "web", 0),
        },
        Request::SetAutoUpdate {
            instance: InstanceId::new("gitlab", "web", 0),
            digest: "sha256:a".into(),
            outcome: AutoUpdateOutcome::Waiting,
            error: None,
            at: 5,
        },
        Request::SetImageChecks {
            checks: BTreeMap::from([(
                "nginx:1".to_string(),
                ImageCheck {
                    latest_digest: Some("sha256:a".into()),
                    checked_at: 5,
                    error: None,
                },
            )]),
            at: 5,
        },
        Request::ClearErrors,
        Request::PutSession {
            key: "k".into(),
            session: SessionRecord {
                subject: "s".into(),
                username: "u".into(),
                admin: false,
                expires_at: 10,
            },
            now: 1,
        },
        Request::RemoveSession { key: "k".into() },
        Request::PruneSessions { now: 5 },
        Request::PushAlertSignal(sample_signal("s1")),
        Request::TakeAlertSignals {
            ids: vec!["s1".into()],
        },
        Request::PutAlert(sample_alert("a1", AlertState::Active, 10)),
        Request::RemoveAlert { id: "a1".into() },
        Request::PruneAlerts { before: 5, max: 10 },
        Request::SetDestinationHealth {
            name: "discord".into(),
            health: DestinationHealth {
                ok: false,
                at: 3,
                error: Some("boom".into()),
                consecutive_failures: 2,
            },
        },
        Request::RemoveDestinationHealth {
            name: "discord".into(),
        },
        Request::PutProxySession {
            key: "k".into(),
            session: ProxySessionRecord {
                host: "a.example.com".into(),
                subject: "s".into(),
                username: "u".into(),
                groups: vec!["g".into()],
                expires_at: 10,
                parent: String::new(),
            },
            now: 1,
        },
        Request::RemoveProxySession { key: "k".into() },
        Request::PutProxyToken {
            key: "t".into(),
            token: ProxyTokenRecord {
                host: "a.example.com".into(),
                subject: "s".into(),
                username: "u".into(),
                groups: Vec::new(),
                session_expires_at: 10,
                expires_at: 5,
                parent: String::new(),
            },
            now: 1,
        },
        Request::RedeemProxyToken {
            token_key: "t".into(),
            host: "a.example.com".into(),
            session_key: "k".into(),
            now: 2,
        },
        Request::RecordCrashMove {
            instance: InstanceId::new("gitlab", "web", 0),
            from: "n1".into(),
            at: 5,
        },
        Request::NoOp,
        Request::ClearEmergency,
        Request::SetPrioritized {
            stack: "s".into(),
            on: true,
        },
        Request::Served {
            drive: "fast".into(),
            op: crate::served::ServedOp::RequestFailback,
        },
        Request::SetStackTerraform {
            stack: "s".into(),
            modules: BTreeMap::from([("web".to_string(), "x".to_string())]),
        },
        Request::PutTerraformState {
            key: "s/web".into(),
            sealed: "c2VhbGVk".into(),
            key_fingerprint: "fp".into(),
            expect_serial: 3,
        },
        Request::RecordTerraformRun {
            run: crate::terraform::TerraformRun {
                id: "r1".into(),
                mode: crate::terraform::TerraformMode::Apply,
                trigger: "change".into(),
                node: "n1".into(),
                instance: InstanceId::new("s", "web", 0),
                fingerprint: "f".into(),
                started_at: 1,
                finished_at: Some(2),
                outcome: crate::terraform::TerraformOutcome::Failed { error: "e".into() },
                log_tail: "l".into(),
            },
        },
        Request::PutTerraformOutputs {
            key: "s/web".into(),
            run_id: "r1".into(),
            secrets: vec![crate::terraform::TerraformSecretOutput {
                name: "sec".into(),
                sealed: "c2VhbGVk".into(),
                key_fingerprint: "fp".into(),
            }],
            configs: vec![crate::terraform::TerraformConfigOutput {
                name: "cfg".into(),
                content: "Zm9v".into(),
            }],
            claims: vec![crate::terraform::OutputObject::config("c2")],
        },
        Request::RequestTerraformRun {
            key: "s/web".into(),
            mode: crate::terraform::TerraformMode::Plan,
        },
    ];

    for req in requests {
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert!(
            !matches!(back, Request::Unknown(_)),
            "variant round-tripped as Unknown: {json}"
        );
        let value = serde_json::to_value(&req).unwrap();
        let tag = match &value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Object(m) => m.keys().next().unwrap().clone(),
            other => panic!("untagged request shape {other}"),
        };
        assert!(
            KNOWN_REQUEST_VARIANTS.contains(&tag.as_str()),
            "{tag} missing from KNOWN_REQUEST_VARIANTS"
        );
        assert_eq!(back, req);
    }
}

#[test]
fn cluster_state_ignores_an_unknown_top_level_field() {
    // Guards against `#[serde(deny_unknown_fields)]`: a new field from a newer node must stay ignorable by an older one.
    let mut value = serde_json::to_value(ClusterState::default()).unwrap();
    value.as_object_mut().unwrap().insert(
        "some_field_from_the_future".into(),
        serde_json::json!({"anything": true}),
    );

    let parsed: Result<ClusterState, _> = serde_json::from_value(value);
    assert!(parsed.is_ok(), "{:?}", parsed.err());
}

#[test]
fn state_round_trips_through_json() {
    let mut state = cluster();
    assign(&mut state, "n1", &["fast"]);
    let encoded = serde_json::to_vec(&state).unwrap();
    let decoded: ClusterState = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(state, decoded);
}
