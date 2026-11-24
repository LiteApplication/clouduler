use super::*;

#[test]
fn drive_reference_must_exist() {
    let raw = "services:\n  a:\n    image: x\n    volumes:\n      - bogus/data:/data\n";
    let err = stack(raw).validate(&cluster()).unwrap_err();
    assert!(err.to_string().contains("services.a.volumes[0]"));
    assert!(err.to_string().contains("no drive named bogus"));
}

#[test]
fn node_pin_must_exist() {
    let raw = "services:\n  a:\n    image: x\n    x-clouduler: { node: bogus }\n";
    let err = stack(raw).validate(&cluster()).unwrap_err();
    assert!(err.to_string().contains("services.a.x-clouduler.node"));
    assert!(err.to_string().contains("no node named bogus"));
}

#[test]
fn whole_drive_mount_on_a_multi_holder_drive_is_rejected() {
    // "bulk" is implemented by both n1 and n3 in the reference cluster.
    let raw = "services:\n  a:\n    image: x\n    volumes:\n      - bulk/.:/data\n    x-clouduler: { node: n1 }\n";
    let err = stack(raw).validate(&cluster()).unwrap_err();
    assert!(err.to_string().contains("services.a.volumes[0]"));
    assert!(err
        .to_string()
        .contains("can be served by more than one node"));
}

fn single_holder_cluster() -> ClusterConfig {
    // Drop n2's "fast" so n1 is the drive's only holder.
    let yaml = CLUSTER_YAML.replace(
        "      wan:\n        domain: n2.example.net # will use CNAME\n    drives:\n      fast: /srv/clouduler/fast\n",
        "      wan:\n        domain: n2.example.net # will use CNAME\n    drives: {}\n",
    );
    ClusterConfig::parse(&yaml).expect("patched cluster is valid")
}

#[test]
fn whole_drive_mount_without_a_pin_is_rejected() {
    let raw = "services:\n  a:\n    image: x\n    volumes:\n      - fast/.:/data\n";
    let err = stack(raw).validate(&single_holder_cluster()).unwrap_err();
    assert!(err.to_string().contains("services.a.volumes[0]"));
    assert!(err.to_string().contains("requires pinning"));
    assert!(err.to_string().contains("x-clouduler.node: n1"));
}

#[test]
fn whole_drive_mount_pinned_to_the_wrong_node_is_rejected() {
    let raw = "services:\n  a:\n    image: x\n    volumes:\n      - fast/.:/data\n    x-clouduler: { node: n3 }\n";
    let err = stack(raw).validate(&single_holder_cluster()).unwrap_err();
    assert!(err.to_string().contains("only n1 holds the real directory"));
}

#[test]
fn whole_drive_mount_pinned_to_the_sole_holder_is_accepted() {
    let raw = "services:\n  a:\n    image: x\n    volumes:\n      - fast/.:/data\n    x-clouduler: { node: n1 }\n";
    stack(raw)
        .validate(&single_holder_cluster())
        .expect("pinned to the drive's only holder");
}

#[test]
fn placement_constraint_node_must_exist() {
    let raw = r#"
services:
  a:
    image: x
    deploy:
      placement:
        constraints: ["node.hostname==bogus"]
"#;
    let err = stack(raw).validate(&cluster()).unwrap_err();
    assert!(err.to_string().contains("deploy.placement.constraints[0]"));
}
