#![allow(unused_imports)]
mod network;
mod placement;
mod service;
mod syntax;

use super::*;
use crate::cluster::ClusterConfig;
use crate::names::is_dns_label;
use crate::stack::deploy::MAX_REPLICAS;

const CLUSTER_YAML: &str = include_str!("../../../../../../examples/cluster.yaml");

fn cluster() -> ClusterConfig {
    ClusterConfig::parse(CLUSTER_YAML).unwrap()
}

fn stack(raw: &str) -> StackFile {
    StackFile::parse(raw).expect("stack parses")
}

#[test]
fn stack_name_check() {
    for ok in [
        "a",
        "web",
        "my-stack",
        "my_stack",
        "a1",
        "0day",
        &"a".repeat(63),
    ] {
        assert!(validate_stack_name(ok).is_ok(), "{ok} should be valid");
    }
    for bad in [
        "",
        "a/b",
        "/a",
        "..",
        "../x",
        "a b",
        "Web",
        "-web",
        "web-",
        "_web",
        "web_",
        "a:b",
        "a.b",
        &"a".repeat(64),
    ] {
        assert!(
            validate_stack_name(bad).is_err(),
            "{bad:?} should be invalid"
        );
    }
}

#[test]
fn dns_label_check() {
    assert!(is_dns_label("gitlab"));
    assert!(is_dns_label("gitlab-ce"));
    assert!(!is_dns_label("Gitlab"));
    assert!(!is_dns_label("-gitlab"));
    assert!(!is_dns_label("gitlab-"));
    assert!(!is_dns_label("gitlab_ce"));
}
