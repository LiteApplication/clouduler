use super::*;

#[test]
fn a_container_network_validates() {
    let cfg = with_container_access(
        "[lan, wan, tailscale]",
        &format!("{CTN_OK}      hosts: [n1]\n      proxy: true\n      open_ports:\n        - {{ port: 2222, target_port: 22 }}\n        - {{ port: 2222, protocol: udp }}\n"),
    )
    .expect("valid");
    let (name, c) = cfg.container_networks().next().unwrap();
    assert_eq!(name, "ctn");
    assert_eq!(c.open_ports[0].effective_target_port(), 22);
    assert_eq!(c.open_ports[1].effective_target_port(), 2222);
    assert_eq!(cfg.container_router_of("ts", "ts"), Some("ctn"));
    assert_eq!(cfg.container_router_of("ts", "other"), None);
    assert_eq!(cfg.access_of("ctn"), ["lan", "wan", "tailscale"]);
    let yaml = serde_yaml::to_string(&cfg).unwrap();
    assert!(yaml.contains("access:"), "{yaml}");
    let again = ClusterConfig::parse(&yaml).unwrap();
    assert_eq!(again, cfg);
}

#[test]
fn reaches_is_reflexive_public_implicit_and_directed() {
    let cfg = with_container_access("[lan]", CTN_OK).unwrap();
    assert!(cfg.reaches("ctn", "ctn"));
    assert!(cfg.reaches("ctn", "lan"));
    assert!(!cfg.reaches("lan", "ctn"));
    // Public targets are implicit, even for a network with no access.
    assert!(cfg.reaches("lan", "wan"));
    assert!(!cfg.reaches("lan", "tailscale"));
    assert!(cfg.access_of("lan").is_empty());
    assert!(cfg.access_of("nope").is_empty());
}

#[test]
fn access_is_checked() {
    let cfg = lan_access("[tailscale, wan]").expect("valid");
    assert!(cfg.reaches("lan", "tailscale"));
    assert!(!cfg.reaches("tailscale", "lan"));
    assert_eq!(cfg.access_of("lan"), ["tailscale", "wan"]);
    assert!(lan_access("[]").is_ok());
    // An internal network may carry one too.
    assert!(parse(&[(
        TS_LINE,
        "  tailscale: { type: internal, subnet: 10.10.0.0/24, domain: int.example.com, access: [lan] }",
    )])
    .is_ok());

    let err = parse(&[(
        WAN_ANCHOR,
        "  wan:       { type: public,   domain: example.com, access: [lan] }",
    )])
    .unwrap_err()
    .to_string();
    assert!(err.contains("access is only allowed"), "{err}");
    let err = lan_access("[nope]").unwrap_err().to_string();
    assert!(err.contains("no network named nope"), "{err}");
    let err = lan_access("[lan]").unwrap_err().to_string();
    assert!(err.contains("reaches itself"), "{err}");
    let err = lan_access("[tailscale, tailscale]")
        .unwrap_err()
        .to_string();
    assert!(err.contains("more than once"), "{err}");
    let err = parse(&[(
        WAN_ANCHOR,
        &format!("{WAN_ANCHOR}\n  vpn: {{ type: egress }}"),
    ), (
        LAN_LINE,
        "  lan:       { type: private,  subnet: 192.168.1.0/24, domain: lan.example.com, access: [vpn] }",
    )])
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("only internal, private, public and container"),
        "{err}"
    );
}

#[test]
fn a_container_block_is_required_and_exclusive() {
    let err = parse(&[(
        WAN_ANCHOR,
        &format!("{WAN_ANCHOR}\n  ctn: {{ type: container }}"),
    )])
    .unwrap_err()
    .to_string();
    assert!(err.contains("needs a container block"), "{err}");
    let err = parse(&[(
        "  lan:       { type: private,  subnet: 192.168.1.0/24, domain: lan.example.com }",
        "  lan:       { type: private,  subnet: 192.168.1.0/24, domain: lan.example.com, container: { stack: a, service: b } }",
    )])
    .unwrap_err()
    .to_string();
    assert!(err.contains("must not declare a container block"), "{err}");
}

#[test]
fn a_container_network_needs_a_subnet_may_have_a_domain_and_takes_no_node_address() {
    let err = parse(&[(
        WAN_ANCHOR,
        &format!(
            "{WAN_ANCHOR}\n  ctn: {{ type: container, container: {{ stack: a, service: b }} }}"
        ),
    )])
    .unwrap_err()
    .to_string();
    assert!(err.contains("needs an IPv4 subnet"), "{err}");
    let cfg = parse(&[(
        WAN_ANCHOR,
        &format!("{WAN_ANCHOR}\n  ctn: {{ type: container, subnet: 100.64.0.0/10, domain: x.example.com, container: {{ stack: a, service: b }} }}"),
    )])
    .expect("a container network may have a domain");
    assert_eq!(cfg.networks["ctn"].domain.as_deref(), Some("x.example.com"));
    // A second network may not claim the same domain.
    let err = parse(&[(
        WAN_ANCHOR,
        &format!("{WAN_ANCHOR}\n  ctn: {{ type: container, subnet: 100.64.0.0/10, domain: example.com, container: {{ stack: a, service: b }} }}"),
    )])
    .unwrap_err()
    .to_string();
    assert!(err.contains("already claimed"), "{err}");
    let err = parse(&[
        (
            WAN_ANCHOR,
            &format!("{WAN_ANCHOR}\n  ctn: {{ type: container, subnet: 100.64.0.0/10, container: {{ stack: a, service: b }} }}"),
        ),
        ("      tailscale: 10.10.0.2\n", "      tailscale: 10.10.0.2\n      ctn: 1.2.3.4\n"),
    ])
    .unwrap_err()
    .to_string();
    assert!(err.contains("takes no node address"), "{err}");
}

#[test]
fn container_hosts_and_access_are_checked() {
    assert!(ctn_err(&format!("{CTN_OK}      hosts: [nope]\n")).contains("no node named nope"));
    assert!(ctn_err(&format!("{CTN_OK}      hosts: [n1, n1]\n")).contains("more than once"));
    let access_err = |access: &str| {
        with_container_access(access, CTN_OK)
            .unwrap_err()
            .to_string()
    };
    assert!(access_err("[nope]").contains("no network named nope"));
    assert!(access_err("[ctn]").contains("reaches itself"));
    let err = parse(&[(
        WAN_ANCHOR,
        &format!("{WAN_ANCHOR}\n  vpn: {{ type: egress }}\n  ctn: {{ type: container, subnet: 100.64.0.0/10, access: [vpn], container: {{ stack: a, service: b }} }}"),
    )])
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("only internal, private, public and container"),
        "{err}"
    );
}

#[test]
fn container_open_ports_may_not_repeat() {
    let err = ctn_err(&format!(
        "{CTN_OK}      open_ports:\n        - {{ port: 22 }}\n        - {{ port: 22, target_port: 2222 }}\n"
    ));
    assert!(err.contains("22/tcp"), "{err}");
}

#[test]
fn container_router_names_must_not_be_empty() {
    assert!(ctn_err("      stack: \"\"\n      service: ts\n").contains("router stack"));
    assert!(ctn_err("      stack: ts\n      service: \"\"\n").contains("router service"));
}

#[test]
fn container_subnet_may_not_sit_inside_the_underlay() {
    for bad in [
        "10.10.0.0/24",
        "10.10.0.128/25",
        "192.168.1.0/28",
        "10.244.3.0/24",
        "10.244.0.0/16",
        "10.245.1.0/24",
        "10.246.0.0/20",
    ] {
        let err = with_container_at(bad, CTN_OK).unwrap_err().to_string();
        assert!(err.contains("lies inside"), "{bad}: {err}");
    }
    // Containing or merely adjacent ranges are fine.
    for ok in ["0.0.0.0/0", "10.0.0.0/8", "192.168.0.0/16", "172.16.0.0/12"] {
        with_container_at(ok, CTN_OK).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
}

#[test]
fn container_subnet_must_be_ipv4() {
    let err = with_container_at("fd00::/8", CTN_OK)
        .unwrap_err()
        .to_string();
    assert!(err.contains("IPv4 subnet"), "{err}");
}

#[test]
fn container_routing_tables_may_not_collide() {
    // Find two names whose (reply, dst) tables overlap.
    let mut seen: std::collections::BTreeMap<u32, String> = Default::default();
    let mut pair = None;
    'outer: for i in 0.. {
        let name = format!("c{i}");
        let (r, d) = crate::netalloc::container_table_ids(&name);
        for t in [r, d] {
            if let Some(other) = seen.get(&t) {
                if *other != name {
                    pair = Some((other.clone(), name.clone()));
                    break 'outer;
                }
            }
        }
        seen.insert(r, name.clone());
        seen.insert(d, name);
    }
    let (a, b) = pair.unwrap();
    let block = "subnet: 100.64.0.0/10, container: { stack: ts, service: ts }";
    let err = parse(&[(
        WAN_ANCHOR,
        &format!("{WAN_ANCHOR}\n  {a}: {{ type: container, {block} }}\n  {b}: {{ type: container, {block} }}"),
    )])
    .unwrap_err()
    .to_string();
    assert!(
        err.contains(&a) && err.contains(&b) && err.contains("routing table"),
        "{err}"
    );
}

#[test]
fn container_packet_marks_may_not_collide() {
    let mut seen: std::collections::BTreeMap<u32, String> = Default::default();
    let mut pair = None;
    for i in 0.. {
        let name = format!("m{i}");
        let mark = crate::netalloc::container_mark(&name);
        if let Some(other) = seen.insert(mark, name.clone()) {
            pair = Some((other, name));
            break;
        }
    }
    let (a, b) = pair.unwrap();
    let block = "subnet: 100.64.0.0/10, container: { stack: ts, service: ts }";
    let err = parse(&[(
        WAN_ANCHOR,
        &format!("{WAN_ANCHOR}\n  {a}: {{ type: container, {block} }}\n  {b}: {{ type: container, {block} }}"),
    )])
    .unwrap_err()
    .to_string();
    // The tables may collide first for some pairs; either refusal is right.
    assert!(
        err.contains(&a)
            && err.contains(&b)
            && (err.contains("packet mark") || err.contains("routing table")),
        "{err}"
    );
}

#[test]
fn a_default_route_container_network_classifies_no_client() {
    let cfg = with_container_at("0.0.0.0/0", CTN_OK).expect("0.0.0.0/0 is allowed");
    for ip in [
        "8.8.8.8",
        "100.64.1.1",
        "10.10.0.7",
        "192.168.1.9",
        "172.20.0.1",
    ] {
        let ip: std::net::IpAddr = ip.parse().unwrap();
        assert!(!cfg.ip_in_network("ctn", ip), "{ip}");
        assert_ne!(cfg.client_network(ip), Some("ctn"), "{ip}");
    }
    assert_eq!(cfg.client_network("8.8.8.8".parse().unwrap()), Some("wan"));
    assert_eq!(
        cfg.client_network("10.10.0.7".parse().unwrap()),
        Some("tailscale")
    );
}
