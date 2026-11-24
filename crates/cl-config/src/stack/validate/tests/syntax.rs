use super::*;

fn tf_stack(tf: &str) -> std::result::Result<super::StackFile, crate::error::ConfigError> {
    super::StackFile::parse(&format!(
        "services:\n  a:\n    image: x\n    x-clouduler:\n      terraform:\n{tf}"
    ))
}

#[test]
fn terraform_bindings_are_validated() {
    tf_stack("        secrets: [{source: s, var: pw}]\n        configs: [{source: c, var: cfg}, {source: d, path: d.json}]\n        vars: {url: x}\n").unwrap();
    for bad in [
        "        vars: {\"1x\": a}\n",
        "        secrets: [{source: s, var: pw}]\n        vars: {pw: a}\n",
        "        configs: [{source: c}]\n",
        "        configs: [{source: c, path: /etc/x}]\n",
        "        configs: [{source: c, path: ../x}]\n",
        "        configs: [{source: c, path: main.tf}]\n",
        "        configs: [{source: c, path: s.tfstate}]\n",
        "        configs: [{source: c, path: a}, {source: d, path: a}]\n",
        "        configs: [{source: c, var: v}, {source: d, var: v}]\n",
    ] {
        assert!(tf_stack(bad).is_err(), "{bad}");
    }
}

#[test]
fn terraform_outputs_are_validated() {
    tf_stack("        outputs:\n          - {output: client_id, config: oidc-id}\n          - {output: client_secret, secret: oidc-secret}\n").unwrap();
    for bad in [
        // not an identifier
        "        outputs: [{output: \"1x\", secret: s}]\n",
        // neither / both targets
        "        outputs: [{output: o}]\n",
        "        outputs: [{output: o, secret: s, config: c}]\n",
        // bad object names
        "        outputs: [{output: o, secret: \"a/b\"}]\n",
        "        outputs: [{output: o, config: \".hidden\"}]\n",
        // duplicate output / duplicate target
        "        outputs: [{output: o, secret: a}, {output: o, secret: b}]\n",
        "        outputs: [{output: o, secret: a}, {output: p, secret: a}]\n",
        // feedback loop with an input binding
        "        secrets: [{source: s, var: pw}]\n        outputs: [{output: o, secret: s}]\n",
        "        configs: [{source: c, var: v}]\n        outputs: [{output: o, config: c}]\n",
        "        outputs: [{output: o, other: x}]\n",
    ] {
        assert!(tf_stack(bad).is_err(), "{bad}");
    }
    // a secret and a config may share a name: they are different kinds
    tf_stack("        outputs: [{output: o, secret: a}, {output: p, config: a}]\n").unwrap();
    // an input of the other kind is not a loop
    tf_stack(
        "        secrets: [{source: s, var: pw}]\n        outputs: [{output: o, config: s}]\n",
    )
    .unwrap();
}

#[test]
fn terraform_outputs_may_not_overwrite_bundled_files() {
    let spec = |extra: &str| {
        super::StackFile::parse(&format!(
            "{extra}services:\n  a:\n    image: x\n    x-clouduler:\n      terraform:\n        outputs: [{{output: o, secret: s}}, {{output: p, config: c}}]\n"
        ))
    };
    spec("").unwrap();
    // declared without a file: an external reference, allowed
    spec("secrets:\n  s:\nconfigs:\n  c:\n").unwrap();
    assert!(spec("secrets:\n  s:\n    file: ./s\n").is_err());
    assert!(spec("configs:\n  c:\n    file: ./c\n").is_err());
}

#[test]
fn terraform_outputs_do_not_count_as_requirements() {
    let s =
        tf_stack("        outputs: [{output: o, secret: s}, {output: p, config: c}]\n").unwrap();
    let r = s.requirements();
    assert!(r.secrets.is_empty() && r.configs.is_empty());
}

#[test]
fn terraform_defaults_to_enabled() {
    let s = tf_stack("        vars: {a: b}\n").unwrap();
    let tf = s.services["a"]
        .x_clouduler
        .as_ref()
        .unwrap()
        .terraform
        .as_ref()
        .unwrap();
    assert!(tf.enabled);
    let s = tf_stack("        enabled: false\n").unwrap();
    assert!(
        !s.services["a"]
            .x_clouduler
            .as_ref()
            .unwrap()
            .terraform
            .as_ref()
            .unwrap()
            .enabled
    );
}

#[test]
fn clouduler_docker_socket_must_be_read_only() {
    let socket = |extra: &str| {
        format!("services:\n  scan:\n    image: x\n    x-clouduler:\n      docker_socket:\n        target: /var/run/docker.sock\n        mode: clouduler\n{extra}")
    };
    let err = super::StackFile::parse(&socket("")).unwrap_err();
    assert!(err.to_string().contains("read_only: true"), "{err}");
    super::StackFile::parse(&socket("        read_only: true\n")).unwrap();
    // The default mode keeps allowing a read-write socket.
    super::StackFile::parse(
        "services:\n  ci:\n    image: x\n    x-clouduler:\n      docker_socket:\n        target: /var/run/docker.sock\n",
    )
    .unwrap();
}

#[test]
fn sync_hooks_must_name_a_service_action() {
    let ok = "services:\n  db:\n    image: x\n    x-clouduler:\n      actions:\n        Flush: { cmd: sync }\n      volume_sync:\n        data: { before: Flush }\n";
    super::StackFile::parse(ok).unwrap();
    let bad = ok.replace("before: Flush", "before: Nope");
    assert!(super::StackFile::parse(&bad).is_err());
    let stack_level = "x-clouduler:\n  volume_sync:\n    data: { after: Flush }\nservices:\n  db:\n    image: x\n";
    assert!(super::StackFile::parse(stack_level).is_err());
}

#[test]
fn a_standalone_action_cannot_be_a_sync_hook() {
    let ok = "services:\n  db:\n    image: x\n    x-clouduler:\n      actions:\n        Flush: { cmd: sync, standalone: true }\n      volume_sync:\n        data: { before: Flush }\n";
    assert!(super::StackFile::parse(ok).is_err());
    super::StackFile::parse(&ok.replace(", standalone: true", "")).unwrap();
}

#[test]
fn an_action_image_needs_standalone() {
    let ok = "services:\n  db:\n    image: x\n    x-clouduler:\n      actions:\n        Fix: { cmd: ls, standalone: true, image: 'postgres:16' }\n";
    let f = super::StackFile::parse(ok).unwrap();
    let a = &f.services["db"].x_clouduler.as_ref().unwrap().actions["Fix"];
    assert_eq!(a.image.as_deref(), Some("postgres:16"));
    assert!(super::StackFile::parse(&ok.replace(", standalone: true", "")).is_err());
    assert!(super::StackFile::parse(&ok.replace("postgres:16", "a b")).is_err());
}

#[test]
fn volume_sync_enabled_is_rejected_with_an_actionable_error() {
    let svc = "services:\n  db:\n    image: x\n    x-clouduler:\n      volume_sync:\n        data: { enabled: false }\n";
    let err = super::StackFile::parse(svc).unwrap_err().to_string();
    assert!(err.contains("volume_sync.data.enabled"), "{err}");
    assert!(err.contains("interval"), "{err}");
    let stack = "x-clouduler:\n  volume_sync:\n    data: { enabled: true }\nservices:\n  db:\n    image: x\n";
    assert!(super::StackFile::parse(stack).is_err());
    // The interval alone is fine.
    let ok = "services:\n  db:\n    image: x\n    x-clouduler:\n      volume_sync:\n        data: { interval: 1h }\n";
    super::StackFile::parse(ok).unwrap();
}

#[test]
fn stop_grace_period_must_be_nonzero_and_bounded() {
    let with = |g: &str| {
        StackFile::parse(&format!(
            "services:\n  web:\n    image: x\n    stop_grace_period: {g}\n"
        ))
    };
    for bad in ["0s", "11m"] {
        let err = with(bad).unwrap_err();
        assert!(
            err.to_string().contains("services.web.stop_grace_period"),
            "{bad}"
        );
    }
    assert!(with("1s").is_ok());
    assert!(with("10m").is_ok());
}

#[test]
fn absurd_replica_counts_are_rejected() {
    let err = StackFile::parse(
        "services:\n  web:\n    image: x\n    deploy:\n      replicas: 4000000000\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("services.web.deploy.replicas"));
    assert!(err.to_string().contains("maximum"));
    assert!(StackFile::parse(&format!(
        "services:\n  web:\n    image: x\n    deploy:\n      replicas: {MAX_REPLICAS}\n"
    ))
    .is_ok());
}

#[test]
fn device_entries_and_requests_are_checked() {
    let spec = |devices: &str, reservation: &str| {
        format!(
            "services:\n  web:\n    image: x\n    devices: {devices}\n    deploy:\n      resources:\n        reservations:\n          devices:\n            - {reservation}\n"
        )
    };
    StackFile::parse(&spec(
        "[\"/dev/dri:/dev/dri\"]",
        "{capabilities: [gpu], count: 1}",
    ))
    .unwrap();
    for (devices, reservation, at) in [
        (
            "[\"/etc/passwd\"]",
            "{capabilities: [gpu]}",
            "services.web.devices[0]",
        ),
        (
            "[\"/dev/../etc\"]",
            "{capabilities: [gpu]}",
            "services.web.devices[0]",
        ),
        (
            "[]",
            "{driver: amd, capabilities: [gpu]}",
            "reservations.devices[0]",
        ),
        (
            "[]",
            "{count: 1, device_ids: ['0'], capabilities: [gpu]}",
            "reservations.devices[0]",
        ),
        ("[]", "{count: 1}", "reservations.devices[0]"),
    ] {
        let err = StackFile::parse(&spec(devices, reservation)).unwrap_err();
        assert!(err.to_string().contains(at), "{err}");
    }
}

#[test]
fn an_overflowing_memory_limit_is_rejected() {
    let err = StackFile::parse(
        "services:\n  web:\n    image: x\n    deploy:\n      resources:\n        limits:\n          memory: 99999999999999999999g\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("deploy.resources.limits.memory"));
    let err = StackFile::parse(
        "services:\n  web:\n    image: x\n    deploy:\n      resources:\n        reservations:\n          memory: 17179869184g\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("too large"));
}

#[test]
fn colocate_target_must_exist() {
    let raw = "services:\n  a:\n    image: x\n    x-clouduler:\n      colocate: [bogus]\n";
    let err = StackFile::parse(raw).unwrap_err();
    assert!(err.to_string().contains("services.a.x-clouduler.colocate"));
    assert!(err.to_string().contains("no service named bogus"));
}

#[test]
fn colocate_with_self_is_rejected() {
    let raw = "services:\n  a:\n    image: x\n    x-clouduler:\n      colocate: [a]\n";
    let err = StackFile::parse(raw).unwrap_err();
    assert!(err.to_string().contains("cannot colocate with itself"));
}

#[test]
fn colocate_with_a_real_service_is_accepted() {
    let raw = "services:\n  a:\n    image: x\n    x-clouduler:\n      colocate: [b]\n  b:\n    image: x\n";
    StackFile::parse(raw).expect("valid colocate target");
}

#[test]
fn a_writable_secret_is_rejected() {
    let raw = "services:\n  a:\n    image: x\n    secrets:\n      - source: db_password\n        write: true\n";
    let err = StackFile::parse(raw).unwrap_err();
    assert!(err.to_string().contains("services.a.secrets[0]"));
    assert!(err.to_string().contains("not allowed on a secret"));
}

#[test]
fn a_writable_config_is_accepted() {
    let raw = "services:\n  a:\n    image: x\n    configs:\n      - source: nginx_conf\n        write: true\n";
    StackFile::parse(raw).expect("a writable config is fine");
}

#[test]
fn two_services_may_not_both_write_one_config() {
    let raw = "services:\n  a:\n    image: x\n    configs:\n      - source: app_conf\n        write: true\n  b:\n    image: x\n    configs:\n      - source: app_conf\n        write: true\n";
    let err = StackFile::parse(raw).unwrap_err();
    assert!(err.to_string().contains("services.b.configs[0]"));
    assert!(err.to_string().contains("single writer"));
}

#[test]
fn one_writer_and_a_read_only_reader_is_fine() {
    let raw = "services:\n  a:\n    image: x\n    configs:\n      - source: app_conf\n        write: true\n  b:\n    image: x\n    configs:\n      - source: app_conf\n";
    StackFile::parse(raw).expect("one writer, one reader");
}

#[test]
fn a_replicated_service_may_not_write_a_config() {
    let raw = "services:\n  a:\n    image: x\n    deploy:\n      replicas: 2\n    configs:\n      - source: app_conf\n        write: true\n";
    let err = StackFile::parse(raw).unwrap_err();
    assert!(err.to_string().contains("2 replicas"));
}
