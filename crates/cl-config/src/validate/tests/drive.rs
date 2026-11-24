use super::*;

#[test]
fn drive_references_must_resolve() {
    let err = parse(&[("drives: { fast: /mnt/fast }", "drives: { bulk: /mnt/bulk }")]).unwrap_err();
    assert!(err.to_string().contains("no drive named bulk"));
}

#[test]
fn drive_paths_must_be_absolute() {
    let err = parse(&[("fast: /mnt/fast", "fast: mnt/fast")]).unwrap_err();
    assert!(err.to_string().contains("absolute path"));
}

#[test]
fn unimplemented_drive_is_allowed() {
    // Scheduling already rejects a node lacking the drive; no need to block staging.
    parse(&[(
        "  fast: { sync_interval: 5m }",
        "  fast: { sync_interval: 5m }\n  bulk: { sync_interval: 5m }",
    )])
    .unwrap();
}

#[test]
fn nodes_implementing_filters_on_every_drive() {
    let cfg = ClusterConfig::parse(BASE).unwrap();
    assert_eq!(cfg.nodes_implementing(&["fast".into()]).count(), 2);
    assert_eq!(
        cfg.nodes_implementing(&["fast".into(), "bulk".into()])
            .count(),
        0
    );
}

#[test]
fn data_dir_defaults_and_overrides_per_node() {
    let cfg = ClusterConfig::parse(BASE).expect("valid");
    assert_eq!(cfg.data_dir, std::path::PathBuf::from("/var/lib/clouduler"));
    assert_eq!(cfg.nodes["n1"].data_dir(&cfg), cfg.data_dir);

    let cfg = parse(&[(
        "    role: worker",
        "    data_dir: /mnt/big/clouduler\n    role: worker",
    )])
    .expect("valid");
    assert_eq!(
        cfg.nodes["n2"].data_dir(&cfg),
        std::path::Path::new("/mnt/big/clouduler")
    );
    // The other node still falls back to the cluster default.
    assert_eq!(cfg.nodes["n1"].data_dir(&cfg), cfg.data_dir);
}

#[test]
fn data_dirs_must_be_absolute() {
    let err = parse(&[(
        "    role: worker",
        "    data_dir: relative/path\n    role: worker",
    )])
    .unwrap_err();
    assert!(err.to_string().contains("absolute path"));
}

#[test]
fn a_served_drive_without_a_serving_node_is_allowed() {
    // The daemon already treats a served drive with no server as "nothing to do".
    let cfg = parse(&[("fast: { sync_interval: 5m }", "fast: { mode: served }")]).expect("valid");
    assert!(cfg.drives["fast"].served_by.is_empty());
}

#[test]
fn max_parallel_syncs_defaults_to_one_and_cannot_be_zero() {
    let cfg = parse(&[(
        "fast: { sync_interval: 5m }",
        "fast: { sync_interval: 5m, max_parallel_syncs: 3 }",
    )])
    .expect("valid");
    assert_eq!(cfg.drives["fast"].max_parallel_syncs, 3);
    let cfg = parse(&[]).expect("valid");
    assert_eq!(cfg.drives["fast"].max_parallel_syncs, 1);
    let err = parse(&[(
        "fast: { sync_interval: 5m }",
        "fast: { sync_interval: 5m, max_parallel_syncs: 0 }",
    )])
    .unwrap_err();
    assert!(err.to_string().contains("max_parallel_syncs"));
}

#[test]
fn a_replicated_drive_rejects_a_serving_node() {
    let err = parse(&[(
        "fast: { sync_interval: 5m }",
        "fast: { sync_interval: 5m, served_by: [n1] }",
    )])
    .unwrap_err();
    assert!(err.to_string().contains("only a served drive"));
}

#[test]
fn the_serving_node_must_implement_the_drive() {
    let err = parse(&[(
        "fast: { sync_interval: 5m }",
        "fast: { mode: served, served_by: [nowhere] }",
    )])
    .unwrap_err();
    assert!(err.to_string().contains("no node named nowhere"));
}

#[test]
fn a_served_drive_rejects_a_duplicate_server() {
    let err = parse(&[(
        "fast: { sync_interval: 5m }",
        "fast: { mode: served, served_by: [n1, n1] }",
    )])
    .unwrap_err();
    assert!(err.to_string().contains("is listed more than once"));
}

#[test]
fn a_served_drive_is_accepted() {
    let cfg = parse(&[(
        "fast: { sync_interval: 5m }",
        "fast: { mode: served, served_by: [n1] }",
    )])
    .expect("valid");
    assert!(cfg.drives["fast"].is_served());
    assert_eq!(cfg.drives["fast"].served_by, vec!["n1".to_string()]);
}

#[test]
fn a_served_drive_accepts_multiple_backup_servers() {
    let cfg = parse(&[(
        "fast: { sync_interval: 5m }",
        "fast: { mode: served, served_by: [n1, n2] }",
    )])
    .expect("valid");
    assert_eq!(
        cfg.drives["fast"].served_by,
        vec!["n1".to_string(), "n2".to_string()]
    );
}
