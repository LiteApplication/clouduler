use super::*;

#[test]
fn rpc_uses_the_internal_address() {
    let cfg = ClusterConfig::parse(BASE).unwrap();
    let addr = cfg.rpc_addr(&cfg.nodes["n1"]).unwrap();
    assert_eq!(addr.ip().to_string(), "10.10.0.1");
    assert_eq!(addr.port(), DEFAULT_RPC_PORT);
}

#[test]
fn node_addresses_resolve_per_network() {
    let cfg = ClusterConfig::parse(BASE).unwrap();
    let n1 = &cfg.nodes["n1"];
    assert_eq!(n1.ip_on("lan").unwrap().to_string(), "192.168.1.1");
    assert_eq!(
        n1.public_on("wan").unwrap().ipv4.unwrap().to_string(),
        "203.0.113.5"
    );
    assert!(n1.ip_on("wan").is_none());
    assert_eq!(
        cfg.nodes["n2"].public_on("wan").unwrap().domain.as_deref(),
        Some("n2.example.net")
    );
}

#[test]
fn every_node_needs_an_internal_address() {
    let err = parse(&[("      tailscale: 10.10.0.2\n", "")]).unwrap_err();
    assert!(err.to_string().contains("address on the internal network"));
}

#[test]
fn node_ip_must_sit_in_the_network_subnet() {
    let err = parse(&[("tailscale: 10.10.0.2", "tailscale: 10.99.0.2")]).unwrap_err();
    assert!(err.to_string().contains("outside the tailscale subnet"));
}

#[test]
fn the_internal_subnet_and_every_node_address_can_move_in_one_edit() {
    // The use case: a cluster bootstrapped on LAN addresses and later put
    // on a tailnet. Nothing about the old addresses may be required.
    let cfg = parse(&[
        ("subnet: 10.10.0.0/24, ", "subnet: 100.64.0.0/10, "),
        ("tailscale: 10.10.0.1", "tailscale: 100.64.0.11"),
        ("tailscale: 10.10.0.2", "tailscale: 100.64.0.12"),
    ])
    .expect("a consistent re-addressing is valid");
    assert_eq!(
        cfg.rpc_addr(&cfg.nodes["n1"]).unwrap().ip().to_string(),
        "100.64.0.11"
    );
}

#[test]
fn moving_only_the_subnet_leaves_the_old_addresses_inconsistent() {
    let err = parse(&[("subnet: 10.10.0.0/24, ", "subnet: 100.64.0.0/10, ")]).unwrap_err();
    assert!(err.to_string().contains("outside the tailscale subnet"));
}

#[test]
fn internal_ips_must_be_unique() {
    let err = parse(&[("tailscale: 10.10.0.2", "tailscale: 10.10.0.1")]).unwrap_err();
    assert!(err.to_string().contains("used by another node"));
}

#[test]
fn address_form_must_match_the_network_kind() {
    let err = parse(&[("lan: 192.168.1.1", "lan: { ipv4: 192.168.1.1 }")]).unwrap_err();
    assert!(err.to_string().contains("takes a bare IP address"));

    let err = parse(&[(
        "wan: { ipv4: 203.0.113.5, ipv6: \"2001:db8::5\" }",
        "wan: 203.0.113.5",
    )])
    .unwrap_err();
    assert!(err
        .to_string()
        .contains("takes an ipv4, ipv6 and/or domain"));
}

#[test]
fn public_address_must_not_be_empty() {
    let err = parse(&[("wan: { domain: n2.example.net }", "wan: {}")]).unwrap_err();
    assert!(err.to_string().contains("at least one of ipv4"));
}

#[test]
fn unknown_network_reference_is_rejected() {
    let err = parse(&[("lan: 192.168.1.1", "wifi: 192.168.1.1")]).unwrap_err();
    assert!(err.to_string().contains("no network named wifi"));
}

#[test]
fn cluster_needs_a_manager() {
    let err = parse(&[("role: manager", "role: worker")]).unwrap_err();
    assert!(err.to_string().contains("role manager"));
}

#[test]
fn a_client_belongs_to_the_network_its_address_is_in() {
    let cfg = ClusterConfig::parse(BASE).unwrap();
    let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
    assert!(cfg.ip_in_network("lan", ip("192.168.1.40")));
    assert!(!cfg.ip_in_network("lan", ip("203.0.113.9")));
    assert!(cfg.ip_in_network("wan", ip("203.0.113.9")));
    assert!(cfg.ip_in_network("wan", ip("2001:db8::9")));
    // A private, tailnet or overlay address is never a public client.
    assert!(!cfg.ip_in_network("wan", ip("192.168.1.40")));
    assert!(!cfg.ip_in_network("wan", ip("10.10.0.7")));
    assert!(!cfg.ip_in_network("wan", ip("100.64.0.1")));
    assert!(!cfg.ip_in_network("wan", ip("127.0.0.1")));
    assert!(!cfg.ip_in_network("wan", ip("fd00::1")));
    assert!(cfg.ip_in_network("lan", ip("::ffff:192.168.1.40")));
}

#[test]
fn a_client_is_classified_by_its_address() {
    let cfg = ClusterConfig::parse(BASE).unwrap();
    let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
    assert_eq!(cfg.client_network(ip("192.168.1.40")), Some("lan"));
    assert_eq!(cfg.client_network(ip("10.10.0.7")), Some("tailscale"));
    assert_eq!(cfg.client_network(ip("203.0.113.9")), Some("wan"));
    assert_eq!(cfg.client_network(ip("127.0.0.1")), None);
}
