use super::*;

#[test]
fn a_managed_network_validates_and_round_trips() {
    let cfg = managed("192.168.32.0/24", "192.168.32.2", "192.168.32.3").expect("valid");
    assert!(cfg.is_managed_network("mgd"));
    assert!(!cfg.is_managed_network("lan"));
    assert_eq!(cfg.managed_networks().len(), 1);
    let yaml = serde_yaml::to_string(&cfg).unwrap();
    assert!(yaml.contains("managed: true"));
    assert_eq!(ClusterConfig::parse(&yaml).unwrap(), cfg);
    // An unmanaged network never serializes the flag.
    let plain = ClusterConfig::parse(BASE).unwrap();
    assert!(!serde_yaml::to_string(&plain).unwrap().contains("managed"));
}

#[test]
fn managed_needs_a_private_network_with_an_ipv4_subnet() {
    let err = parse(&[(
        "  wan:       { type: public,   domain: example.com }",
        "  wan:       { type: public,   domain: example.com, managed: true }",
    )])
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("only a private network can be managed"),
        "{err}"
    );
    let err = parse(&[(
        "  tailscale: { type: internal, subnet: 10.10.0.0/24, domain: int.example.com }",
        "  tailscale: { type: internal, subnet: 10.10.0.0/24, domain: int.example.com, managed: true }",
    )])
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("only a private network can be managed"),
        "{err}"
    );
    let err = managed_err("fd00::/64", "192.168.32.2", "192.168.32.3");
    assert!(err.contains("IPv4"), "{err}");
}

#[test]
fn managed_prefix_length_is_bounded() {
    for bad in ["10.50.0.0/15", "192.168.32.0/30", "192.168.32.0/31"] {
        let err = managed_err(bad, "192.168.32.2", "192.168.32.3");
        assert!(err.contains("prefix length"), "{bad}: {err}");
    }
    managed("192.168.32.0/29", "192.168.32.2", "192.168.32.3").expect("/29");
    managed("172.20.0.0/16", "172.20.0.2", "172.20.0.3").expect("/16");
    let err = managed_err("192.168.32.4/24", "192.168.32.2", "192.168.32.3");
    assert!(err.contains("host bits"), "{err}");
}

#[test]
fn managed_subnet_may_not_overlap_anything() {
    for bad in [
        "192.168.1.0/24",
        "192.168.0.0/16",
        "10.10.0.0/24",
        "10.244.0.0/16",
        "10.245.0.0/24",
        "10.246.0.0/20",
    ] {
        let err = managed_err(bad, "192.168.32.2", "192.168.32.3");
        assert!(err.contains("overlaps"), "{bad}: {err}");
    }
}

#[test]
fn managed_node_addresses_are_checked() {
    let err = managed_err("192.168.32.0/24", "192.168.33.2", "192.168.32.3");
    assert!(err.contains("outside"), "{err}");
    let err = managed_err("192.168.32.0/24", "192.168.32.0", "192.168.32.3");
    assert!(err.contains("network or broadcast"), "{err}");
    let err = managed_err("192.168.32.0/24", "192.168.32.2", "192.168.32.255");
    assert!(err.contains("network or broadcast"), "{err}");
    let err = managed_err("192.168.32.0/24", "192.168.32.1", "192.168.32.3");
    assert!(err.contains("gateway"), "{err}");
    let err = managed_err("192.168.32.0/24", "192.168.32.2", "192.168.32.2");
    assert!(err.contains("used by another node"), "{err}");
}

#[test]
fn a_private_network_may_be_listed_without_an_address() {
    let cfg = parse(&[("lan: 192.168.1.1", "lan:")]).expect("valid");
    let n1 = &cfg.nodes["n1"];
    assert_eq!(n1.network.get("lan"), Some(&NodeAddress::None));
    assert!(n1.ip_on("lan").is_none());
    assert!(n1.is_addressless_on("lan"));
    let yaml = serde_yaml::to_string(&cfg).unwrap();
    let again: ClusterConfig = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(
        again.nodes["n1"].network.get("lan"),
        Some(&NodeAddress::None)
    );
    parse(&[("lan: 192.168.1.1", "lan: null")]).expect("explicit null");
}

#[test]
fn only_a_private_network_may_be_listed_without_an_address() {
    let err = parse(&[("tailscale: 10.10.0.1", "tailscale:")]).unwrap_err();
    assert!(
        err.to_string()
            .contains("needs an IP address on every node"),
        "{err}"
    );
    let err = parse(&[("wan: { ipv4: 203.0.113.5, ipv6: \"2001:db8::5\" }", "wan:")]).unwrap_err();
    assert!(err.to_string().contains("public network takes"), "{err}");
}

#[test]
fn a_managed_network_may_be_listed_without_an_address_for_access() {
    let cfg = managed("192.168.32.0/24", "~", "192.168.32.3").expect("valid");
    assert!(cfg.nodes["n1"].is_addressless_on("mgd"));
    // It gets an automatic address, reserved like an explicit one.
    let auto = cfg.node_address_on("n1", "mgd").expect("auto address");
    let reserved = cfg.managed_reserved("mgd");
    assert!(reserved.contains(&"192.168.32.3".parse().unwrap()));
    assert!(reserved.contains(&auto));
    // Still needs an IPv4 internal address for the tunnel.
    let err = parse(&[
        (
            "  wan:       { type: public,   domain: example.com }",
            "  wan:       { type: public,   domain: example.com }\n  mgd:       { type: private, subnet: 192.168.32.0/24, managed: true }",
        ),
        ("tailscale: { type: internal, subnet: 10.10.0.0/24", "tailscale: { type: internal, subnet: fd00::/64"),
        ("      lan: 192.168.1.1\n", "      lan: 192.168.1.1\n      mgd:\n"),
        ("tailscale: 10.10.0.1", "tailscale: fd00::1"),
        ("tailscale: 10.10.0.2", "tailscale: fd00::2"),
    ])
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("no IPv4 address on the internal network"),
        "{err}"
    );
}

#[test]
fn managed_nodes_get_stable_automatic_addresses() {
    let cfg = managed("192.168.32.0/24", "~", "~").expect("valid");
    let all = cfg.managed_node_addresses("mgd");
    assert_eq!(all.len(), 2);
    let a1 = cfg.node_address_on("n1", "mgd").unwrap();
    let a2 = cfg.node_address_on("n2", "mgd").unwrap();
    assert_ne!(a1, a2);
    let subnet: ipnet::Ipv4Net = "192.168.32.0/24".parse().unwrap();
    for a in [a1, a2] {
        assert!(subnet.contains(&a));
        assert_ne!(a, managed_gateway(subnet));
        assert_ne!(a, subnet.network());
        assert_ne!(a, subnet.broadcast());
    }
    // Same config, same answer; an explicit address is left alone and avoided.
    assert_eq!(cfg.node_address_on("n1", "mgd"), Some(a1));
    let mixed = managed("192.168.32.0/24", &a2.to_string(), "~").expect("valid");
    assert_eq!(mixed.node_address_on("n1", "mgd"), Some(a2));
    assert_ne!(mixed.node_address_on("n2", "mgd"), Some(a2));
    assert_eq!(cfg.node_address_on("n1", "lan"), None);
    assert_eq!(cfg.addressed_nodes_on("mgd"), vec!["n1", "n2"]);
}

#[test]
fn managed_open_ports_and_proxy_round_trip() {
    let cfg = managed_with(
        ", proxy: true, open_ports: [{ port: 2222, target_port: 22, stack: git, service: ssh }, { port: 53, protocol: udp, stack: dns, service: dns }]",
        "192.168.32.2",
        "~",
    )
    .expect("valid");
    let ports = cfg.managed_open_ports("mgd");
    assert_eq!(ports.len(), 2);
    assert_eq!(ports[0].effective_target_port(), 22);
    assert_eq!(ports[1].effective_target_port(), 53);
    assert!(cfg.managed_proxy("mgd"));
    assert!(!cfg.managed_proxy("lan"));
    assert!(cfg.managed_open_ports("lan").is_empty());
    let again: ClusterConfig = serde_yaml::from_str(&serde_yaml::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        again.networks["mgd"].open_ports,
        cfg.networks["mgd"].open_ports
    );
    assert!(again.networks["mgd"].proxy);
    let plain = managed("192.168.32.0/24", "192.168.32.2", "~").unwrap();
    let yaml = serde_yaml::to_string(&plain.networks["mgd"]).unwrap();
    assert!(
        !yaml.contains("open_ports") && !yaml.contains("proxy"),
        "{yaml}"
    );
}

#[test]
fn managed_open_ports_are_validated() {
    let err = |extra: &str| {
        managed_with(extra, "192.168.32.2", "~")
            .unwrap_err()
            .to_string()
    };
    let dup = err(", open_ports: [{ port: 22, stack: a, service: b }, { port: 22, target_port: 2, stack: a, service: c }]");
    assert!(dup.contains("22/tcp is listed more than once"), "{dup}");
    let zero = err(", open_ports: [{ port: 0, stack: a, service: b }]");
    assert!(zero.contains("port 0"), "{zero}");
    let blank = err(", open_ports: [{ port: 22, stack: '', service: b }]");
    assert!(blank.contains("stack and service"), "{blank}");
    let web = err(", proxy: true, open_ports: [{ port: 443, stack: a, service: b }]");
    assert!(web.contains("reverse proxy"), "{web}");
    // The same port over another protocol is fine.
    managed_with(", open_ports: [{ port: 22, stack: a, service: b }, { port: 22, protocol: udp, stack: a, service: b }]", "~", "~").expect("tcp and udp");
    managed_with(
        ", open_ports: [{ port: 443, stack: a, service: b }]",
        "~",
        "~",
    )
    .expect("443 without proxy");
}

#[test]
fn open_ports_and_proxy_are_only_for_managed_networks() {
    let err = parse(&[(
        "  wan:       { type: public,   domain: example.com }",
        "  wan:       { type: public,   domain: example.com }\n  p: { type: private, subnet: 192.168.40.0/24, proxy: true }",
    )])
    .unwrap_err()
    .to_string();
    assert!(err.contains("only allowed on a managed network"), "{err}");
    let err = parse(&[(
        "  wan:       { type: public,   domain: example.com }",
        "  wan:       { type: public,   domain: example.com }\n  p: { type: private, subnet: 192.168.40.0/24, open_ports: [{ port: 1, stack: a, service: b }] }",
    )])
    .unwrap_err()
    .to_string();
    assert!(err.contains("open_ports"), "{err}");
}

#[test]
fn a_managed_address_needs_an_ipv4_internal_address_on_the_node() {
    let err = parse(&[
        (
            "  wan:       { type: public,   domain: example.com }",
            "  wan:       { type: public,   domain: example.com }\n  mgd:       { type: private, subnet: 192.168.32.0/24, managed: true }",
        ),
        ("tailscale: { type: internal, subnet: 10.10.0.0/24", "tailscale: { type: internal, subnet: fd00::/64"),
        ("      lan: 192.168.1.1\n", "      lan: 192.168.1.1\n      mgd: 192.168.32.2\n"),
        ("tailscale: 10.10.0.1", "tailscale: fd00::1"),
        ("tailscale: 10.10.0.2", "tailscale: fd00::2"),
    ])
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("no IPv4 address on the internal network"),
        "{err}"
    );
    assert!(err.contains("nodes."), "{err}");
}

/// BASE plus a managed network `mgd` (with a domain) where n1 and n2 hold
/// the given addresses (`~` for none), with the dashboard on `web` and the
/// login offered on `login_networks`.
fn managed_portal(n1: &str, n2: &str, web: &str, login_networks: &str) -> Result<ClusterConfig> {
    let auth = format!(
        "proxy:\n  auth:\n    host: login\n    networks: [{login_networks}]\n    issuer: https://id.example.com\n    client_id: c\n"
    );
    parse(&[
        (
            "  wan:       { type: public,   domain: example.com }",
            "  wan:       { type: public,   domain: example.com }\n  mgd:       { type: private, subnet: 192.168.32.0/24, managed: true, domain: m.example.com }",
        ),
        ("      lan: 192.168.1.1\n", &format!("      lan: 192.168.1.1\n      mgd: {n1}\n")),
        ("      tailscale: 10.10.0.2\n", &format!("      tailscale: 10.10.0.2\n      mgd: {n2}\n")),
        ("web: { address: cloud, network: tailscale }", &format!("web: {{ address: cloud, network: {web} }}")),
        ("  zones: [example.com]\n", &format!("  zones: [example.com]\n{auth}")),
    ])
}

#[test]
fn the_dashboard_and_login_page_may_sit_on_a_managed_network_with_an_addressed_node() {
    // One addressed node is enough, whichever it is.
    let cfg = managed_portal("192.168.32.2", "~", "mgd", "lan, mgd").expect("n1 holds an address");
    // A node listing the network without an address gets an automatic one.
    assert_eq!(cfg.addressed_nodes_on("mgd"), vec!["n1", "n2"]);
    managed_portal("~", "192.168.32.3", "mgd", "mgd").expect("n2 holds an address");
    managed_portal("~", "~", "mgd", "lan, mgd").expect("automatic addresses");
    let both = managed_portal("192.168.32.2", "192.168.32.3", "mgd", "lan, mgd").expect("both");
    assert_eq!(both.addressed_nodes_on("mgd"), vec!["n1", "n2"]);
}

#[test]
fn the_reserved_addresses_cover_network_gateway_broadcast_and_nodes() {
    let cfg = managed("192.168.32.0/24", "192.168.32.2", "192.168.32.4").unwrap();
    // .0 network, .1 gateway, .2 and .4 nodes, .255 broadcast.
    let reserved = cfg.managed_reserved("mgd");
    for ip in [
        "192.168.32.0",
        "192.168.32.1",
        "192.168.32.2",
        "192.168.32.4",
        "192.168.32.255",
    ] {
        assert!(reserved.contains(&ip.parse().unwrap()), "{ip}");
    }
}
