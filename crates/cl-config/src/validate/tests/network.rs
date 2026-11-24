use super::*;

#[test]
fn only_one_internal_network_is_allowed() {
    let err = parse(&[("lan:       { type: private", "lan:       { type: internal")]).unwrap_err();
    assert!(err.to_string().contains("only one internal network"));

    let err = parse(&[("tailscale: { type: internal", "tailscale: { type: private")]).unwrap_err();
    assert!(err.to_string().contains("exactly one internal network"));
}

#[test]
fn subnetted_networks_need_a_subnet() {
    let err = parse(&[("subnet: 10.10.0.0/24, ", "")]).unwrap_err();
    assert!(err.to_string().contains("internal network needs a subnet"));
}

#[test]
fn public_networks_reject_a_subnet() {
    let err = parse(&[(
        "wan:       { type: public,",
        "wan:       { type: public, subnet: 1.0.0.0/8,",
    )])
    .unwrap_err();
    assert!(err.to_string().contains("must not declare a subnet"));
}

#[test]
fn wireguard_network_takes_a_bare_interface_name() {
    let cfg = parse(&[(
        "  wan:       { type: public,   domain: example.com }",
        "  wan:       { type: public,   domain: example.com }\n  vpn:       { type: wireguard }",
    )])
    .expect("valid");
    assert_eq!(cfg.networks["vpn"].kind, NetworkKind::Wireguard);

    let err = parse(&[
        (
            "  wan:       { type: public,   domain: example.com }",
            "  wan:       { type: public,   domain: example.com }\n  vpn:       { type: wireguard }",
        ),
        ("      tailscale: 10.10.0.1", "      tailscale: 10.10.0.1\n      vpn: { ipv4: 1.2.3.4 }"),
    ])
    .unwrap_err();
    assert!(err.to_string().contains("takes a bare host interface name"));
}

/// Two distinct names whose derived value (octet or table id) collides,
/// found by search so the test does not hard-code a hash.
fn colliding_names<K: PartialEq + Ord>(key: impl Fn(&str) -> K) -> (String, String) {
    let mut seen: std::collections::BTreeMap<K, String> = Default::default();
    for i in 0.. {
        let name = format!("net{i}");
        let k = key(&name);
        if let Some(other) = seen.get(&k) {
            return (other.clone(), name);
        }
        seen.insert(k, name);
    }
    unreachable!()
}

#[test]
fn two_egress_networks_may_not_share_a_derived_subnet() {
    let (a, b) = colliding_names(crate::netalloc::subnet_octet);
    let err = with_networks("egress", &[&a, &b]).unwrap_err().to_string();
    assert!(err.contains(&a) && err.contains(&b), "{err}");
    assert!(err.contains("subnet"), "{err}");
}

#[test]
fn two_nat_networks_may_not_share_a_derived_subnet() {
    let (a, b) = colliding_names(crate::netalloc::subnet_octet);
    let err = with_networks("nat", &[&a, &b]).unwrap_err().to_string();
    assert!(err.contains(&a) && err.contains(&b), "{err}");
}

#[test]
fn an_egress_and_a_nat_network_never_collide_on_subnet() {
    let (a, b) = colliding_names(crate::netalloc::subnet_octet);
    let extra = format!("\n  {a}: {{ type: egress }}\n  {b}: {{ type: nat }}");
    parse(&[(
        "  wan:       { type: public,   domain: example.com }",
        &format!("  wan:       {{ type: public,   domain: example.com }}{extra}"),
    )])
    .expect("separate pools");
}

#[test]
fn two_egress_networks_may_not_share_a_routing_table() {
    // Table collisions are far likelier than subnet ones to also be
    // subnet-distinct, so search for names that only collide on the table.
    let mut seen: std::collections::BTreeMap<u32, String> = Default::default();
    let mut pair = None;
    for i in 0.. {
        let name = format!("t{i}");
        let id = crate::netalloc::egress_table_id(&name);
        if let Some(other) = seen.get(&id) {
            if crate::netalloc::subnet_octet(other) != crate::netalloc::subnet_octet(&name) {
                pair = Some((other.clone(), name));
                break;
            }
        } else {
            seen.insert(id, name);
        }
    }
    let (a, b) = pair.unwrap();
    let err = with_networks("wireguard", &[&a, &b])
        .unwrap_err()
        .to_string();
    assert!(err.contains(&a) && err.contains(&b), "{err}");
    assert!(err.contains("routing table"), "{err}");
}

#[test]
fn host_interface_names_are_validated() {
    for bad in ["a/b", "has space", "abcdefghijklmnop", "../x"] {
        let err = parse(&[
            (
                "  wan:       { type: public,   domain: example.com }",
                "  wan:       { type: public,   domain: example.com }\n  vpn: { type: egress }",
            ),
            (
                "      tailscale: 10.10.0.1",
                &format!("      tailscale: 10.10.0.1\n      vpn: \"{bad}\""),
            ),
        ])
        .unwrap_err();
        assert!(
            err.to_string().contains("interface") || err.to_string().contains(bad),
            "{bad}: {err}"
        );
    }
}

#[test]
fn nat_network_takes_no_node_address() {
    let cfg = parse(&[(
        "  wan:       { type: public,   domain: example.com }",
        "  wan:       { type: public,   domain: example.com }\n  home:      { type: nat }",
    )])
    .expect("valid");
    assert_eq!(cfg.networks["home"].kind, NetworkKind::Nat);

    let err = parse(&[
        (
            "  wan:       { type: public,   domain: example.com }",
            "  wan:       { type: public,   domain: example.com }\n  home:      { type: nat }",
        ),
        (
            "      tailscale: 10.10.0.1",
            "      tailscale: 10.10.0.1\n      home: eth0",
        ),
    ])
    .unwrap_err();
    assert!(err.to_string().contains("takes no node address"));
}

#[test]
fn wireguard_interface_open_ports_round_trip_and_stay_bare_when_empty() {
    let cfg = parse(&[
        (
            "  wan:       { type: public,   domain: example.com }",
            "  wan:       { type: public,   domain: example.com }\n  vpn:       { type: wireguard }",
        ),
        (
            "      tailscale: 10.10.0.1",
            "      tailscale: 10.10.0.1\n      vpn: { interface: wg0, open_ports: [{ port: 51413 }, { port: 53, protocol: udp }] }",
        ),
    ])
    .expect("valid");
    let addr = cfg.nodes["n1"].network["vpn"]
        .as_interface_address()
        .expect("interface address");
    assert_eq!(addr.interface, "wg0");
    assert_eq!(addr.open_ports.len(), 2);
    assert!(addr.open_ports.contains(&crate::cluster::OpenPort {
        port: 51413,
        protocol: crate::stack::Protocol::Tcp,
    }));

    // A bare interface (no ports declared) round trips as a plain string.
    let yaml = serde_yaml::to_string(&cfg).unwrap();
    assert!(yaml.contains("wan:") && yaml.contains("203.0.113.5"));
    let n2_addr = cfg.nodes["n2"].network.get("tailscale").unwrap();
    let reserialized = serde_yaml::to_string(n2_addr).unwrap();
    assert_eq!(reserialized.trim(), "10.10.0.2");
}

#[test]
fn egress_fallback_round_trips_and_is_rejected_when_it_matches_the_primary() {
    let cfg = parse(&[
        (
            "  wan:       { type: public,   domain: example.com }",
            "  wan:       { type: public,   domain: example.com }\n  vpn:       { type: egress }",
        ),
        (
            "      tailscale: 10.10.0.1",
            "      tailscale: 10.10.0.1\n      vpn: { interface: end0, fallback: { interface: wlan0, health_check_ip: 1.1.1.1 } }",
        ),
    ])
    .expect("valid");
    let addr = cfg.nodes["n1"].network["vpn"]
        .as_interface_address()
        .expect("interface address");
    let fallback = addr.fallback.as_ref().expect("fallback set");
    assert_eq!(fallback.interface, "wlan0");
    assert_eq!(
        fallback.health_check_ip,
        "1.1.1.1".parse::<std::net::IpAddr>().unwrap()
    );

    let yaml = serde_yaml::to_string(&cfg.nodes["n1"].network["vpn"]).unwrap();
    assert!(yaml.contains("fallback"));
    assert!(yaml.contains("wlan0"));

    let err = parse(&[
        (
            "  wan:       { type: public,   domain: example.com }",
            "  wan:       { type: public,   domain: example.com }\n  vpn:       { type: egress }",
        ),
        (
            "      tailscale: 10.10.0.1",
            "      tailscale: 10.10.0.1\n      vpn: { interface: end0, fallback: { interface: end0, health_check_ip: 1.1.1.1 } }",
        ),
    ])
    .unwrap_err();
    assert!(err
        .to_string()
        .contains("different interface than the primary"));
}

#[test]
fn wireguard_network_rejects_a_domain() {
    let err = parse(&[(
        "  wan:       { type: public,   domain: example.com }",
        "  wan:       { type: public,   domain: example.com }\n  vpn:       { type: wireguard, domain: vpn.example.com }",
    )])
    .unwrap_err();
    assert!(err
        .to_string()
        .contains("wireguard network is not addressable"));
}

#[test]
fn network_domain_must_be_covered_by_a_zone() {
    let err = parse(&[("zones: [example.com]", "zones: [other.com]")]).unwrap_err();
    assert!(err
        .to_string()
        .contains("not covered by any configured cloudflare zone"));
}

#[test]
fn publishing_to_dns_is_on_unless_a_private_network_opts_out() {
    let cfg = ClusterConfig::parse(BASE).unwrap();
    assert!(cfg.networks.values().all(|n| n.publish_dns));

    let off = BASE.replace(
        "lan:       { type: private,",
        "lan:       { type: private, publish_dns: false,",
    );
    assert!(!ClusterConfig::parse(&off).unwrap().networks["lan"].publish_dns);

    let bad = BASE.replace(
        "wan:       { type: public,",
        "wan:       { type: public, publish_dns: false,",
    );
    let err = ClusterConfig::parse(&bad).unwrap_err().to_string();
    assert!(err.contains("publish_dns"), "{err}");
}

/// `interval` no longer exists but older cluster.yaml files still have it;
/// they must keep loading.
#[test]
fn backup_tolerates_a_leftover_interval_field() {
    let cfg = with_backup(
        "  repo_url: git@github.com:org/repo.git\n  age_recipient: age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq\n  interval: 30s\n",
    )
    .expect("valid");
    assert_eq!(cfg.backup.expect("backup set").branch, "main");
}
