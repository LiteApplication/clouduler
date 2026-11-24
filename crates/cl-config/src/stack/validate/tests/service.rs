use super::*;

#[test]
fn invalid_service_name_rejected() {
    let raw = "services:\n  Bad_Name:\n    image: x\n";
    let err = StackFile::parse(raw).unwrap_err();
    assert!(err.to_string().contains("services.Bad_Name"));
    assert!(err.to_string().contains("DNS label"));
}

#[test]
fn depends_on_target_must_exist() {
    let raw = "services:\n  a:\n    image: x\n    depends_on: [b]\n";
    let err = StackFile::parse(raw).unwrap_err();
    assert!(err.to_string().contains("services.a.depends_on"));
    assert!(err.to_string().contains("no service named b"));
}

#[test]
fn env_file_is_rejected_in_a_stack_file() {
    let raw = "services:\n  a:\n    image: x\n    env_file: .env\n";
    let err = StackFile::parse(raw).unwrap_err();
    assert!(err.to_string().contains("services.a.env_file"));
    assert!(err.to_string().contains("import it"));

    let raw = "services:\n  a:\n    image: x\n    env_file: [.env, .env.local]\n";
    assert!(StackFile::parse(raw).is_err());
}

#[test]
fn depends_on_self_rejected() {
    let raw = "services:\n  a:\n    image: x\n    depends_on: [a]\n";
    let err = StackFile::parse(raw).unwrap_err();
    assert!(err.to_string().contains("services.a.depends_on"));
    assert!(err.to_string().contains("cannot depend on itself"));
}

#[test]
fn depends_on_cycle_rejected() {
    let raw = "services:\n  a:\n    image: x\n    depends_on: [b]\n  b:\n    image: x\n    depends_on: [c]\n  c:\n    image: x\n    depends_on: [a]\n";
    let err = StackFile::parse(raw).unwrap_err();
    assert!(err
        .to_string()
        .contains("dependency cycle: a -> b -> c -> a"));
}

#[test]
fn depends_on_diamond_is_not_a_cycle() {
    let raw = "services:\n  a:\n    image: x\n    depends_on: [b, c]\n  b:\n    image: x\n    depends_on: [d]\n  c:\n    image: x\n    depends_on: [d]\n  d:\n    image: x\n";
    StackFile::parse(raw).unwrap();
}

#[test]
fn service_network_must_be_declared_in_stack() {
    let raw = "services:\n  a:\n    image: x\n    networks: [wan]\n";
    let err = StackFile::parse(raw).unwrap_err();
    assert!(err.to_string().contains("services.a.networks"));
    assert!(err.to_string().contains("not declared"));
}

#[test]
fn duplicate_published_ports_rejected() {
    let raw = r#"
services:
  a:
    image: x
    ports: ["8080:80"]
  b:
    image: x
    ports: ["8080:81"]
"#;
    let err = StackFile::parse(raw).unwrap_err();
    assert!(err.to_string().contains("already published by"));
}
