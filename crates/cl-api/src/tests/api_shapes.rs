use super::*;

// ---- the api/ui contract ----
//
// Three separate bugs shipped because a field was renamed on one side of the
// wire and not the other: the config rollback countdown (`remaining_seconds`
// vs `seconds_left`), and the overview's quorum banner (`has_quorum` vs
// `quorum`), which showed "Quorum lost" on a healthy cluster for as long as it
// existed. Nothing failed in either language: Rust did not know what the ui
// read, and TypeScript types are erased at runtime, so the ui happily read
// `undefined` and carried on.
//
// This test records the real serialized body of every endpoint the ui reads
// into a fixture the ui's own test suite imports. On the TypeScript side those
// fixtures are *assigned to the interfaces*, so `tsc` fails the build if an
// interface declares a field the server does not actually send. The fixture is
// generated from real responses, so it cannot drift from the server; the
// assignment is checked by the compiler, so it cannot drift from the ui.
//
// Regenerate after an intentional api change:
//
//     UPDATE_API_FIXTURES=1 cargo test -p cl-api api_shapes
const API_SHAPES_FIXTURE: &str = "../../ui/src/api/__fixtures__/api-shapes.json";

/// Pin the values that differ run to run, so the fixture records the shape and
/// not one run's accidents.
///
/// Replacements keep the original JSON type. That matters: the ui assigns these
/// fixtures to its interfaces, and a field typed as a string union (`role`,
/// `type`, `availability`) would stop typechecking if its value were blanked.
/// So this is a short explicit list of volatile keys rather than a blanket
/// "erase every leaf".
fn normalize(value: &mut serde_json::Value, tempdir: &str) {
    /// Random per run: generated ids, hashes, key fingerprints, ephemeral ports.
    const VOLATILE_STRINGS: &[&str] = &[
        "fingerprint",
        "key_fingerprint",
        "subject",
        "rpc_addr",
        "content",
        "id",
        // A live registry lookup (nginx:latest), so it moves whenever upstream republishes.
        "latest_digest",
    ];
    const VOLATILE_NUMBERS: &[&str] = &[
        "state_version",
        // Live readings of the machine the test happens to run on.
        "cpu_percent",
        "cpu_count",
        "load_1m",
        "memory_total_bytes",
        "memory_used_bytes",
        "uptime_seconds",
        "total_bytes",
        "free_bytes",
        "container_count",
        // A timed disk probe: 0 or 1 ms depending on the machine.
        "disk_ms",
    ];

    match value {
        serde_json::Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if VOLATILE_STRINGS.contains(&key.as_str()) && v.is_string() {
                    *v = serde_json::json!("<volatile>");
                } else if VOLATILE_NUMBERS.contains(&key.as_str()) && v.is_number() {
                    *v = serde_json::json!(0);
                } else {
                    normalize(v, tempdir);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                normalize(v, tempdir);
            }
        }
        serde_json::Value::String(text) if text.contains(tempdir) => {
            *text = text.replace(tempdir, "<tmp>");
        }
        _ => {}
    }
}

#[tokio::test]
async fn api_shapes_match_the_fixture_the_ui_typechecks_against() {
    let h = harness().await;

    // Give the config a served drive n0 implements, so `/api/drives` returns
    // a real item instead of `[]` -- an empty array can't prove the item
    // shape, which is why it's still skipped for stacks/instances/certificates
    // below. Served (rather than replicated) exercises `active_server` as a
    // string, not just its `null` case.
    {
        let mut cfg = h.state.cluster_config.write().await;
        cfg.drives.insert(
            "fast".to_string(),
            cl_config::Drive {
                sync_interval: Duration::from_secs(900),
                mode: cl_config::DriveMode::Served,
                served_by: vec!["n0".to_string()],
                failback: Default::default(),
                max_parallel_syncs: 1,
            },
        );
    }
    // A container network whose router is `web.app.0` (assigned below), with
    // an address book entry for the segment, the router and n0, so
    // `/api/networks/{name}/status` returns a real router and node row rather
    // than `null` and `{}`.
    {
        let mut cfg = h.state.cluster_config.write().await;
        cfg.networks.insert(
            "tailnet".to_string(),
            cl_config::Network {
                kind: cl_config::NetworkKind::Container,
                subnet: Some("100.64.0.0/10".parse().unwrap()),
                domain: None,
                publish_dns: true,
                access: Some(vec!["internal".to_string()]),
                managed: false,
                open_ports: Vec::new(),
                proxy: false,
                shortcut: Default::default(),
                container: Some(cl_config::ContainerNetwork {
                    stack: "web".to_string(),
                    service: "app".to_string(),
                    hosts: vec!["n0".to_string()],
                    open_ports: Vec::new(),
                    proxy: false,
                }),
            },
        );
    }
    {
        let segment = cl_net::container_net::container_docker_name("tailnet");
        let mut book = cl_net::Allocations::default();
        book.allocate_network(&segment, &[]).expect("segment");
        book.allocate_ip(&segment, &cl_raft::ids::InstanceId::new("web", "app", 0))
            .expect("router address");
        book.allocate_node_ip(&segment, "n0").expect("node address");
        h.state
            .cluster
            .submit(cl_raft::Request::SetOverlayAllocations {
                allocations: serde_json::to_string(&book).unwrap(),
            })
            .await
            .expect("submit allocations");
    }
    let node = h.state.cluster.state().await.nodes["n0"].clone();
    h.state
        .cluster
        .submit(cl_raft::Request::UpsertNode(NodeState {
            drives: BTreeMap::from([("fast".to_string(), "/mnt/fast".into())]),
            node_pubkey: Some("target".to_string()),
            ..node
        }))
        .await
        .expect("submit node drives");

    // Same reasoning as the drive above, for every other collection `state`
    // carries: an empty map or array can't prove the item shape underneath it
    // still matches what the ui declares, which is exactly how `labels`
    // drifted onto `NodeState` unnoticed. One real item each, so a rename in
    // any of these gets caught the same way.
    h.state
        .cluster
        .submit(cl_raft::Request::DeployStack {
            name: "web".to_string(),
            spec: "services:\n  app:\n    image: nginx\n    volumes:\n      - fast/v0:/data\n  job:\n    image: nginx\n    volumes:\n      - fast/v0:/data\n".to_string(),
            at: 0,
        })
        .await
        .expect("submit stack");
    h.state
        .cluster
        .submit(cl_raft::Request::SetStackObjects {
            name: "web".to_string(),
            configs: BTreeSet::from(["web.conf".to_string()]),
            secrets: BTreeSet::from(["web.secret".to_string()]),
            used_configs: BTreeSet::from(["web.conf".to_string()]),
            used_secrets: BTreeSet::from(["web.secret".to_string()]),
        })
        .await
        .expect("submit stack objects");

    h.state
        .cluster
        .submit(cl_raft::Request::Assign {
            instance: cl_raft::InstanceId::new("web", "app", 0),
            node: "n0".to_string(),
            drives: BTreeSet::new(),
        })
        .await
        .expect("submit assign");

    // A second node to move to, and a second instance whose move failed
    // because the first one (same volume) kept writing: exercises
    // `migration_problem` with a real co-writer.
    h.state
        .cluster
        .submit(cl_raft::Request::UpsertNode(NodeState {
            name: "n1".to_string(),
            id: uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, b"n1"),
            node_pubkey: None,
            ..h.state.cluster.state().await.nodes["n0"].clone()
        }))
        .await
        .expect("submit second node");
    let moving = cl_raft::InstanceId::new("web", "job", 0);
    h.state
        .cluster
        .submit(cl_raft::Request::Assign {
            instance: moving.clone(),
            node: "n0".to_string(),
            drives: BTreeSet::new(),
        })
        .await
        .expect("submit assign job");
    h.state
        .cluster
        .submit(cl_raft::Request::BeginMigration {
            instance: moving.clone(),
            to: "n1".to_string(),
            drives: BTreeSet::new(),
        })
        .await
        .expect("submit begin migration");
    h.state
        .cluster
        .submit(cl_raft::Request::AdvanceMigration {
            instance: moving,
            phase: cl_raft::state::MigrationPhase::Failed {
                reason: "verification failed: fast/v0: target differs after sync (added=[] changed=[\"apps/activity/l10n/fr.json\", \"custom_apps/calendar/js/calendar-main.js\"] removed=[])".to_string(),
            },
        })
        .await
        .expect("submit failed migration");

    let volume = cl_raft::VolumeId::new("fast", "v0");
    h.state
        .cluster
        .submit(cl_raft::Request::UpsertVolume {
            id: volume.clone(),
            primary: "n0".to_string(),
        })
        .await
        .expect("submit volume");
    h.state
        .cluster
        .submit(cl_raft::Request::SetVolumeReplica {
            id: volume,
            node: "n0".to_string(),
            generation: 1,
        })
        .await
        .expect("submit volume replica");

    h.state
        .cluster
        .submit(cl_raft::Request::PutSecret {
            name: "web.secret".to_string(),
            sealed: "c2VhbGVk".to_string(),
            key_fingerprint: "fp".to_string(),
            owner: Some("web".to_string()),
        })
        .await
        .expect("submit secret");

    h.state
        .cluster
        .submit(cl_raft::Request::StoreCertificate(
            cl_raft::CertificateState {
                fqdn: "example.test".to_string(),
                chain_pem: "chain".to_string(),
                key_pem: "key".to_string(),
                not_after: 0,
                issued_at: 0,
            },
        ))
        .await
        .expect("submit certificate");

    h.state
        .cluster
        .submit(cl_raft::Request::UpsertDnsRecord(cl_raft::DnsRecordState {
            fqdn: "example.test".to_string(),
            record_type: "A".to_string(),
            content: "127.0.0.1".to_string(),
            zone: "test".to_string(),
            provider_id: None,
        }))
        .await
        .expect("submit dns record");

    h.state
        .cluster
        .submit(cl_raft::Request::PutWrappedKey {
            node: "n0".to_string(),
            entry: cl_raft::state::WrappedKeyEntry {
                ephemeral_public: "eph".to_string(),
                sealed: "sealed".to_string(),
                key_fingerprint: "fp".to_string(),
                target_pubkey: "target".to_string(),
            },
        })
        .await
        .expect("submit wrapped key");

    // A certificate whose renewal is failing, and a record still waiting to
    // be published: the two non-"ready" row shapes.
    h.state
        .cluster
        .submit(cl_raft::Request::SetCertStatus {
            fqdn: "example.test".to_string(),
            status: cl_raft::ReconcileStatus::Error {
                message: "dns challenge timed out".to_string(),
                at: 0,
            },
        })
        .await
        .expect("submit cert status");
    h.state
        .cluster
        .submit(cl_raft::Request::SetCertStatus {
            fqdn: "new.example.test".to_string(),
            status: cl_raft::ReconcileStatus::Pending,
        })
        .await
        .expect("submit pending cert");
    h.state
        .cluster
        .submit(cl_raft::Request::SetDnsStatus {
            fqdn: "new.example.test".to_string(),
            status: cl_raft::ReconcileStatus::Pending,
        })
        .await
        .expect("submit pending dns");

    // Alerts: one open record with a delivery, one destination with its
    // sealed url and a health reading, so every field of the three alert
    // responses is present at least once.
    {
        use cl_alert::{Destination, DestinationKind};
        let mut settings = cl_alert::AlertSettings::default();
        settings.destinations.push(Destination {
            name: "main".into(),
            enabled: true,
            kind: DestinationKind::Webhook,
            severities: [cl_alert::Severity::Critical].into(),
            digest: false,
            categories: vec![],
            stacks: vec![],
        });
        h.state
            .cluster
            .submit(cl_raft::Request::PutConfig {
                name: cl_alert::settings::SETTINGS_CONFIG.into(),
                content: crate::alerts::settings::encode(&settings),
                owner: None,
            })
            .await
            .expect("submit alert settings");
        let key = h.state.key.read().await.clone().expect("harness key");
        let name = cl_alert::settings::url_secret("main");
        let sealed =
            cl_secret::Sealed::seal(&key, &name, b"https://hooks.example.test/path/TOKEN").unwrap();
        h.state
            .cluster
            .submit(cl_raft::Request::PutSecret {
                name,
                sealed: sealed.blob,
                key_fingerprint: key.fingerprint(),
                owner: None,
            })
            .await
            .expect("submit alert secret");
        h.state
            .cluster
            .submit(cl_raft::Request::SetDestinationHealth {
                name: "main".into(),
                health: cl_raft::alerts::DestinationHealth {
                    ok: false,
                    at: 5,
                    error: Some("HTTP 500".into()),
                    consecutive_failures: 2,
                },
            })
            .await
            .expect("submit destination health");
        h.state
            .cluster
            .submit(cl_raft::Request::PutAlert(cl_raft::alerts::AlertRecord {
                id: "a1".into(),
                event: "node.disk_low".into(),
                dedupe: "node.disk_low:n0".into(),
                category: "nodes".into(),
                severity: cl_config::AlertSeverity::Critical,
                state: cl_raft::alerts::AlertState::Active,
                title: "Disk almost full".into(),
                detail: "95% used".into(),
                subject: "n0".into(),
                stack: Some("web".into()),
                instance: Some("web.app.0".into()),
                node: Some("n0".into()),
                opened_at: 1,
                last_at: 2,
                resolved_at: Some(3),
                count: 2,
                escalated_from: Some(cl_config::AlertSeverity::Warning),
                sent_severity: Some(cl_config::AlertSeverity::Critical),
                resolved_sent: false,
                digested: false,
                links: vec![cl_raft::alerts::AlertLink {
                    label: "Open node".into(),
                    path: "/nodes/n0".into(),
                    method: None,
                }],
                excerpt: vec!["line".into()],
                deliveries: vec![cl_raft::alerts::AlertDelivery {
                    destination: "main".into(),
                    at: 2,
                    ok: false,
                    error: Some("HTTP 500".into()),
                    attempts: 5,
                    phase: "opened".into(),
                    message_id: None,
                }],
            }))
            .await
            .expect("submit alert");
    }

    let mut shapes = serde_json::Map::new();
    for (name, uri) in [
        ("status", "/api/status"),
        ("alerts", "/api/alerts"),
        ("alertCatalog", "/api/alerts/catalog"),
        ("alertSettings", "/api/alerts/settings"),
        ("state", "/api/state"),
        ("identity", "/auth/me"),
        ("clusterConfig", "/api/config"),
        ("networkStatus", "/api/networks/tailnet/status"),
        ("cloudflareTokenStatus", "/api/cloudflare/token"),
        ("oidcClientSecretStatus", "/api/oidc/client-secret"),
        ("clusterKeyStatus", "/api/secrets/key"),
        ("selfUpdateGithubToken", "/api/self-update/github-token"),
        ("selfUpdateRepo", "/api/self-update/repo"),
        ("selfUpdateRun", "/api/self-update/run"),
        ("drives", "/api/drives"),
        // Covered even though it is an array, unlike stacks/instances/certs
        // below: the harness assigns `web.app.0` above, so there is a real
        // item to prove the row shape against.
        ("updates", "/api/updates"),
        ("nodeConnectivity", "/api/nodes/connectivity"),
        ("stacks", "/api/stacks"),
        ("stackDetail", "/api/stacks/web"),
        ("instances", "/api/instances"),
        ("volumes", "/api/volumes"),
        ("certificates", "/api/certificates"),
        ("dns", "/api/dns"),
        ("nodeDetail", "/api/nodes/n0"),
        ("nodeMetrics", "/api/nodes/metrics"),
        ("proxyRoutes", "/api/proxy/routes"),
        ("lastRebalance", "/api/rebalance/last"),
    ] {
        let resp = h
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("authorization", format!("Bearer {}", h.admin_key))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri} did not answer 200");
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{uri} is not json: {e}"));
        shapes.insert(name.to_string(), value);
    }

    // The harness runs in a fresh tempdir and the state version moves as the
    // harness sets itself up, so the values are not stable across runs. Only
    // the *shape* is the contract, so the volatile ones are blanked rather
    // than making the test flaky.
    let mut shapes = serde_json::Value::Object(shapes);
    let tempdir = h
        .state
        .config_path
        .parent()
        .unwrap()
        .to_string_lossy()
        .to_string();
    normalize(&mut shapes, &tempdir);

    let rendered = serde_json::to_string_pretty(&shapes).unwrap() + "\n";
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(API_SHAPES_FIXTURE);

    if std::env::var("UPDATE_API_FIXTURES").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).expect("create fixture dir");
        std::fs::write(&path, &rendered).expect("write fixture");
        return;
    }

    let existing = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}\nregenerate with: UPDATE_API_FIXTURES=1 cargo test -p cl-api api_shapes",
            path.display()
        )
    });

    // Compare parsed, so formatting or key order never fails the test.
    let expected: serde_json::Value = serde_json::from_str(&existing).expect("fixture is json");
    let actual: serde_json::Value = serde_json::from_str(&rendered).expect("rendered is json");
    assert_eq!(
        actual, expected,
        "the api response shape changed.\nIf that was intended, regenerate the fixture the ui \
         typechecks against:\n    UPDATE_API_FIXTURES=1 cargo test -p cl-api api_shapes\n\
         and run the ui tests, which will fail if a TypeScript interface no longer matches."
    );
}
