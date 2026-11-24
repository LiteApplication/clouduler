#![allow(unused_imports)]
mod auth;
mod backup;
mod container;
mod drive;
mod failover;
mod managed;
mod network;
mod node;
mod proxy;

use super::*;
use crate::cluster::{managed_gateway, qualify, NetworkKind, NodeAddress, DEFAULT_RPC_PORT};
use crate::error::ConfigError;
use crate::names::is_within;

const BASE: &str = r#"
cluster: { name: home }
networks:
  tailscale: { type: internal, subnet: 10.10.0.0/24, domain: int.example.com }
  lan:       { type: private,  subnet: 192.168.1.0/24, domain: lan.example.com }
  wan:       { type: public,   domain: example.com }
drives:
  fast: { sync_interval: 5m }
nodes:
  n1:
    id: 3b0f6c4a-3d3f-4a8e-9b8b-2c1e6a2a9f01
    role: manager
    network:
      tailscale: 10.10.0.1
      lan: 192.168.1.1
      wan: { ipv4: 203.0.113.5, ipv6: "2001:db8::5" }
    drives: { fast: /srv/clouduler/fast }
  n2:
    id: 9a5b8b0a-6f3f-4c9c-a9a0-6a2c1e6a2a02
    role: worker
    network:
      tailscale: 10.10.0.2
      wan: { domain: n2.example.net }
    drives: { fast: /mnt/fast }
web: { address: cloud, network: tailscale }
cloudflare:
  zones: [example.com]
"#;

fn parse(patch: &[(&str, &str)]) -> Result<ClusterConfig> {
    let mut raw = BASE.to_string();
    for (from, to) in patch {
        assert!(raw.contains(from), "patch anchor {from} not found");
        raw = raw.replace(from, to);
    }
    ClusterConfig::parse(&raw)
}

#[test]
fn parses_reference_config() {
    let cfg = ClusterConfig::parse(BASE).expect("base config is valid");
    assert_eq!(cfg.cluster.name, "home");
    assert_eq!(cfg.nodes.len(), 2);
    assert_eq!(cfg.managers().count(), 1);
    assert_eq!(cfg.quorum(), 1);
    assert_eq!(cfg.internal_network().unwrap().0, "tailscale");
    assert_eq!(
        cfg.drives["fast"].sync_interval,
        std::time::Duration::from_secs(300)
    );
}

#[test]
fn bare_web_address_gets_the_network_domain() {
    let cfg = ClusterConfig::parse(BASE).unwrap();
    assert_eq!(cfg.web_fqdn(), "cloud.int.example.com");
    let cfg = parse(&[("address: cloud,", "address: ui.lan.example.com,")]).unwrap();
    assert_eq!(cfg.web_fqdn(), "ui.lan.example.com");
}

fn with_networks(kind: &str, names: &[&str]) -> Result<ClusterConfig> {
    let extra: String = names
        .iter()
        .map(|n| format!("\n  {n}: {{ type: {kind} }}"))
        .collect();
    parse(&[(
        "  wan:       { type: public,   domain: example.com }",
        &format!("  wan:       {{ type: public,   domain: example.com }}{extra}"),
    )])
}

#[test]
fn unknown_keys_are_rejected() {
    let err = ClusterConfig::parse(&format!("{BASE}extra: 1\n")).unwrap_err();
    assert!(matches!(err, ConfigError::Parse { .. }));
}

#[test]
fn terraform_section_is_optional_with_defaults() {
    assert!(ClusterConfig::parse(BASE).unwrap().terraform.is_none());
    let cfg = ClusterConfig::parse(&format!("{BASE}terraform: {{}}\n")).unwrap();
    let tf = cfg.terraform.unwrap();
    assert_eq!(tf.image, "ghcr.io/opentofu/opentofu:1.10");
    assert_eq!(tf.timeout, std::time::Duration::from_secs(600));
    assert_eq!(tf.init_network, "bridge");
    let cfg = ClusterConfig::parse(&format!(
        "{BASE}terraform:\n  timeout: 2m\n  init_network: net\n"
    ))
    .unwrap();
    assert_eq!(
        cfg.terraform.unwrap().timeout,
        std::time::Duration::from_secs(120)
    );
    assert!(ClusterConfig::parse(&format!("{BASE}terraform:\n  bogus: 1\n")).is_err());
}

const AUTH: &str = "\nproxy:\n  auth:\n    host: login\n    networks: [lan, wan]\n    issuer: https://id.example.com\n    client_id: c\n";

#[test]
fn qualify_only_touches_bare_labels() {
    assert_eq!(qualify("cloud", Some("example.com")), "cloud.example.com");
    assert_eq!(qualify("a.b.com", Some("example.com")), "a.b.com");
    assert_eq!(qualify("cloud", None), "cloud");
}

#[test]
fn is_within_matches_subdomains_only() {
    assert!(is_within("a.example.com", "example.com"));
    assert!(is_within("example.com", "example.com"));
    assert!(!is_within("notexample.com", "example.com"));
    assert!(!is_within("example.com.evil.net", "example.com"));
}

fn with_backup(patch: &str) -> Result<ClusterConfig> {
    parse(&[(
        "cloudflare:\n  zones: [example.com]",
        &format!("cloudflare:\n  zones: [example.com]\nbackup:\n{patch}"),
    )])
}

const WAN_ANCHOR: &str = "  wan:       { type: public,   domain: example.com }";

fn with_container(block: &str) -> Result<ClusterConfig> {
    with_container_at("100.64.0.0/10", block)
}

fn with_container_at(subnet: &str, block: &str) -> Result<ClusterConfig> {
    parse(&[(
        WAN_ANCHOR,
        &format!("{WAN_ANCHOR}\n  ctn:\n    type: container\n    subnet: \"{subnet}\"\n    container:\n{block}"),
    )])
}

const CTN_OK: &str = "      stack: ts\n      service: ts\n";

fn ctn_err(block: &str) -> String {
    with_container(block).unwrap_err().to_string()
}

const LAN_LINE: &str =
    "  lan:       { type: private,  subnet: 192.168.1.0/24, domain: lan.example.com }";
const TS_LINE: &str =
    "  tailscale: { type: internal, subnet: 10.10.0.0/24, domain: int.example.com }";

fn lan_access(access: &str) -> Result<ClusterConfig> {
    parse(&[(
        LAN_LINE,
        &format!("  lan:       {{ type: private,  subnet: 192.168.1.0/24, domain: lan.example.com, access: {access} }}"),
    )])
}

fn managed(subnet: &str, n1: &str, n2: &str) -> Result<ClusterConfig> {
    parse(&[
        (
            "  wan:       { type: public,   domain: example.com }",
            &format!(
                "  wan:       {{ type: public,   domain: example.com }}\n  mgd:       {{ type: private, subnet: {subnet}, managed: true }}"
            ),
        ),
        ("      lan: 192.168.1.1\n", &format!("      lan: 192.168.1.1\n      mgd: {n1}\n")),
        ("      tailscale: 10.10.0.2\n", &format!("      tailscale: 10.10.0.2\n      mgd: {n2}\n")),
    ])
}

fn managed_err(subnet: &str, n1: &str, n2: &str) -> String {
    managed(subnet, n1, n2).unwrap_err().to_string()
}

fn managed_with(net_extra: &str, n1: &str, n2: &str) -> Result<ClusterConfig> {
    parse(&[
        (
            "  wan:       { type: public,   domain: example.com }",
            &format!(
                "  wan:       {{ type: public,   domain: example.com }}\n  mgd:       {{ type: private, subnet: 192.168.32.0/24, managed: true{net_extra} }}"
            ),
        ),
        ("      lan: 192.168.1.1\n", &format!("      lan: 192.168.1.1\n      mgd: {n1}\n")),
        ("      tailscale: 10.10.0.2\n", &format!("      tailscale: 10.10.0.2\n      mgd: {n2}\n")),
    ])
}
