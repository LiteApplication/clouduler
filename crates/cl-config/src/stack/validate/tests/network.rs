use super::*;

/// Silently ignoring it would mislead about real client addresses.
#[test]
fn proxy_protocol_on_a_udp_port_is_rejected() {
    let raw = r#"
services:
  a:
    image: x
    x-clouduler:
      expose:
        - { port: 53, published: 53, protocol: udp, network: public, proxy_protocol: true }
"#;
    let err = StackFile::parse(raw).unwrap_err();
    assert!(err.to_string().contains("only applies to tcp"), "{err}");
}

#[test]
fn private_shortcut_false_is_accepted_on_any_route_network() {
    let cluster = ClusterConfig::parse(include_str!("../../../../../../examples/cluster.yaml"))
        .expect("reference cluster is valid");
    let spec = |net: &str| {
        format!(
            "networks:\n  {net}: {{}}\nservices:\n  a:\n    image: x\n    networks: [{net}]\n    x-clouduler:\n      proxy:\n        - host: a\n          network: {net}\n          port: 80\n          private_shortcut: false\n"
        )
    };
    for net in ["wan", "lan"] {
        StackFile::parse(&spec(net))
            .unwrap()
            .validate(&cluster)
            .unwrap();
    }
}

#[test]
fn expose_host_needs_a_network_with_a_domain() {
    let cluster = ClusterConfig::parse(include_str!("../../../../../../examples/cluster.yaml"))
        .expect("reference cluster is valid");
    let spec = |net: &str| {
        format!(
            "services:\n  a:\n    image: x\n    x-clouduler:\n      expose:\n        - {{ port: 53, published: 53, network: {net}, host: dns, private_shortcut: false }}\n"
        )
    };
    let file = StackFile::parse(&spec("lan")).unwrap();
    file.validate(&cluster).unwrap();
    let e = &file.services["a"].x_clouduler.as_ref().unwrap().expose[0];
    assert_eq!(e.host.as_deref(), Some("dns"));
    assert!(!e.private_shortcut);

    let mut nodomain = cluster.clone();
    nodomain.networks.get_mut("lan").unwrap().domain = None;
    let err = StackFile::parse(&spec("lan"))
        .unwrap()
        .validate(&nodomain)
        .unwrap_err();
    assert!(err.to_string().contains("no domain"), "{err}");
}

#[test]
fn cloudflare_proxy_is_public_networks_only() {
    let cluster = ClusterConfig::parse(include_str!("../../../../../../examples/cluster.yaml"))
        .expect("reference cluster is valid");
    let spec = |net: &str| {
        format!(
            "networks:\n  {net}: {{}}\nservices:\n  a:\n    image: x\n    networks: [{net}]\n    x-clouduler:\n      proxy:\n        - host: a\n          network: {net}\n          port: 80\n          cloudflare_proxy: true\n"
        )
    };
    StackFile::parse(&spec("wan"))
        .unwrap()
        .validate(&cluster)
        .unwrap();
    let err = StackFile::parse(&spec("lan"))
        .unwrap()
        .validate(&cluster)
        .unwrap_err();
    assert!(err.to_string().contains("cloudflare_proxy"), "{err}");
}

#[test]
fn proxy_protocol_on_a_tcp_port_is_accepted() {
    let raw = r#"
services:
  a:
    image: x
    x-clouduler:
      expose:
        - { port: 25, published: 25, protocol: tcp, network: public, proxy_protocol: true }
"#;
    let file = StackFile::parse(raw).expect("tcp is the supported case");
    let expose = &file.services["a"].x_clouduler.as_ref().unwrap().expose[0];
    assert!(expose.proxy_protocol);
}

/// Must default off: a backend not speaking PROXY protocol sees a malformed request.
#[test]
fn proxy_protocol_defaults_to_off() {
    let raw = r#"
services:
  a:
    image: x
    x-clouduler:
      expose:
        - { port: 25, published: 25, network: public }
"#;
    let file = StackFile::parse(raw).expect("parse");
    assert!(!file.services["a"].x_clouduler.as_ref().unwrap().expose[0].proxy_protocol);
}

#[test]
fn cluster_network_must_exist() {
    let raw = "networks:\n  bogus: {}\nservices:\n  a:\n    image: x\n";
    let err = stack(raw).validate(&cluster()).unwrap_err();
    assert!(err.to_string().contains("networks.bogus"));
}

#[test]
fn proxy_network_must_have_a_domain() {
    let raw = r#"
networks:
  tailscale: {}
services:
  a:
    image: x
    networks: [tailscale]
    x-clouduler:
      proxy:
        - host: a
          network: bogus
          port: 80
"#;
    let err = stack(raw).validate(&cluster()).unwrap_err();
    assert!(err
        .to_string()
        .contains("services.a.x-clouduler.proxy[0].network"));
}

#[test]
fn a_login_can_only_be_asked_for_on_a_network_that_offers_it() {
    let raw = |network: &str| {
        format!(
            "networks:\n  tailscale: {{}}\nservices:\n  a:\n    image: x\n    networks: [tailscale]\n    x-clouduler:\n      proxy:\n        - host: a\n          network: {network}\n          port: 80\n          middlewares: [authenticated]\n"
        )
    };
    let with_auth = |networks: &str| {
        let yaml = CLUSTER_YAML.replace(
            "proxy:\n  enabled: true\n",
            &format!(
                "proxy:\n  enabled: true\n  auth:\n    host: login\n    networks: {networks}\n    issuer: https://sso.example.com/\n    client_id: apps\n"
            ),
        );
        ClusterConfig::parse(&yaml).expect("cluster with proxy.auth is valid")
    };

    // No proxy.auth at all.
    let err = stack(&raw("lan")).validate(&cluster()).unwrap_err();
    assert!(err.to_string().contains("authenticated"), "{err}");
    assert!(err.to_string().contains("no proxy.auth"), "{err}");

    // Offered on lan only: wan is refused, lan is fine.
    let lan_only = with_auth("[lan]");
    let err = stack(&raw("wan")).validate(&lan_only).unwrap_err();
    assert!(err.to_string().contains("does not list it"), "{err}");
    assert!(stack(&raw("lan")).validate(&lan_only).is_ok());
}

#[test]
fn network_with_no_implementing_node_is_rejected() {
    let raw = r#"
networks:
  unreachable: {}
services:
  a:
    image: x
    networks: [unreachable]
"#;
    // Add a network to the reference cluster that no node opts into.
    let yaml = CLUSTER_YAML.replace(
        "  vpn:\n    type: egress\n",
        "  vpn:\n    type: egress\n  unreachable:\n    type: egress\n",
    );
    let cluster = ClusterConfig::parse(&yaml).expect("patched cluster is valid");
    let err = stack(raw).validate(&cluster).unwrap_err();
    assert!(err
        .to_string()
        .contains("no node implements network unreachable"));
}

#[test]
fn a_nat_network_needs_no_implementing_node() {
    let raw = r#"
networks:
  home: {}
services:
  a:
    image: x
    networks: [home]
    x-clouduler: { node: n1 }
"#;
    let yaml = CLUSTER_YAML.replace(
        "  vpn:\n    type: egress\n",
        "  vpn:\n    type: egress\n  home:\n    type: nat\n",
    );
    let cluster = ClusterConfig::parse(&yaml).expect("patched cluster is valid");
    // No node has (or needs) an interface address on `home`, yet the
    // service, even pinned to a specific node, is still accepted.
    stack(raw)
        .validate(&cluster)
        .expect("a nat network is implemented by every node");
}

#[test]
fn a_service_cannot_attach_to_both_an_egress_and_a_nat_network() {
    let raw = r#"
networks:
  vpn: {}
  home: {}
services:
  a:
    image: x
    networks: [vpn, home]
"#;
    let yaml = CLUSTER_YAML.replace(
        "  vpn:\n    type: egress\n",
        "  vpn:\n    type: egress\n  home:\n    type: nat\n",
    );
    let cluster = ClusterConfig::parse(&yaml).expect("patched cluster is valid");
    let err = stack(raw).validate(&cluster).unwrap_err();
    assert!(err
        .to_string()
        .contains("only one egress network is allowed per service"));
}

#[test]
fn expose_port_nobody_has_open_is_rejected() {
    let raw = r#"
services:
  a:
    image: x
    x-clouduler:
      expose:
        - port: 25
          published: 25
          network: wan
"#;
    let err = stack(raw).validate(&cluster()).unwrap_err();
    assert!(err
        .to_string()
        .contains("no node has 25/tcp open on network wan"));
}

#[test]
fn expose_port_a_node_has_open_is_accepted() {
    let raw = r#"
services:
  a:
    image: x
    x-clouduler:
      expose:
        - port: 25
          published: 25
          network: wan
"#;
    let mut cfg = cluster();
    cfg.nodes.get_mut("n1").unwrap().network.insert(
        "wan".to_string(),
        crate::cluster::NodeAddress::Public(crate::cluster::PublicAddress {
            ipv4: Some("203.0.113.9".parse().unwrap()),
            ipv6: None,
            domain: None,
            open_ports: [crate::cluster::OpenPort {
                port: 25,
                protocol: crate::stack::Protocol::Tcp,
            }]
            .into_iter()
            .collect(),
        }),
    );
    stack(raw).validate(&cfg).expect("a node has the port open");
}

#[test]
fn expose_port_a_wireguard_node_has_open_is_accepted() {
    let raw = r#"
services:
  a:
    image: x
    x-clouduler:
      expose:
        - port: 22
          published: 51413
          network: vpn
"#;
    let yaml = CLUSTER_YAML.replace(
        "  vpn:\n    type: egress\n",
        "  vpn:\n    type: wireguard\n",
    );
    let mut cfg = ClusterConfig::parse(&yaml).expect("patched cluster is valid");

    let err = stack(raw).validate(&cfg).unwrap_err();
    assert!(err
        .to_string()
        .contains("no node has 51413/tcp open on network vpn"));

    cfg.nodes.get_mut("n1").unwrap().network.insert(
        "vpn".to_string(),
        crate::cluster::NodeAddress::Interface(crate::cluster::InterfaceAddress {
            interface: "wg-vpn".to_string(),
            open_ports: [crate::cluster::OpenPort {
                port: 51413,
                protocol: crate::stack::Protocol::Tcp,
            }]
            .into_iter()
            .collect(),
            fallback: None,
        }),
    );
    stack(raw)
        .validate(&cfg)
        .expect("n1's tunnel has the port open");
}

fn net_stack(allowed: &str) -> StackFile {
    stack(&format!(
        "services:\n  web:\n    image: x\n    networks: [tailscale]\nnetworks:\n  tailscale:\n{allowed}"
    ))
}

#[test]
fn allowed_is_absent_by_default_and_empty_means_deny_all() {
    let f = net_stack("    {}\n");
    assert_eq!(f.networks["tailscale"].as_ref().unwrap().allowed, None);
    let f = net_stack("    allowed: []\n");
    assert_eq!(
        f.networks["tailscale"].as_ref().unwrap().allowed,
        Some(vec![])
    );
    f.validate(&cluster()).unwrap();
}

#[test]
fn allowed_entries_validate_on_the_overlay_network() {
    let f = net_stack(
        "    allowed:\n      - cloud.home.liteapp.fr\n      - \"service.home.liteapp.fr:22\"\n      - \"*.example.org\"\n",
    );
    f.validate(&cluster()).unwrap();
}

#[test]
fn a_bad_allowed_entry_is_a_parse_error() {
    let err = StackFile::parse(
        "services:\n  web:\n    image: x\nnetworks:\n  tailscale:\n    allowed: [\"bad host\"]\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("not a valid hostname"), "{err}");
}

#[test]
fn allowed_is_accepted_on_egress_networks_with_an_overlay() {
    let f = stack(
        "services:\n  web:\n    image: x\n    networks: [tailscale, vpn]\nnetworks:\n  tailscale: {}\n  vpn:\n    allowed: [a.example.org, \"*.b.example.org:443\", 1.2.3.4]\n",
    );
    f.validate(&cluster()).unwrap();
}

#[test]
fn allowed_on_egress_needs_an_overlay_for_the_resolver() {
    let f = stack(
        "services:\n  web:\n    image: x\n    networks: [vpn]\nnetworks:\n  vpn:\n    allowed: [a.example.org]\n",
    );
    let err = f.validate(&cluster()).unwrap_err();
    assert!(err.to_string().contains("networks.vpn.allowed"), "{err}");
}

#[test]
fn a_wildcard_port_is_refused_off_wan_networks() {
    let f = stack(
        "services:\n  web:\n    image: x\n    networks: [tailscale]\nnetworks:\n  tailscale:\n    allowed: [\"*.example.org:443\"]\n",
    );
    let err = f.validate(&cluster()).unwrap_err();
    assert!(err.to_string().contains("cannot take a port"), "{err}");
}

#[test]
fn port_entries_are_refused_off_the_overlay_network() {
    // tailscale (internal) is the overlay; wan only carries routes.
    let f = stack(
        "services:\n  web:\n    image: x\n    networks: [tailscale, wan]\nnetworks:\n  tailscale: {}\n  wan:\n    allowed: [\"auth.example.org:22\"]\n",
    );
    let err = f.validate(&cluster()).unwrap_err();
    assert!(err.to_string().contains("networks.wan.allowed"), "{err}");
    assert!(err.to_string().contains("tailscale"), "{err}");

    // The same list with hostnames only is fine.
    let f = stack(
        "services:\n  web:\n    image: x\n    networks: [tailscale, wan]\nnetworks:\n  tailscale: {}\n  wan:\n    allowed: [auth.example.org]\n",
    );
    f.validate(&cluster()).unwrap();
}

#[test]
fn the_overlay_network_prefers_subnetted_over_public() {
    let f = stack(
        "services:\n  web:\n    image: x\nnetworks:\n  wan: {}\n  tailscale: {}\n  vpn: {}\n",
    );
    assert_eq!(f.overlay_network(&cluster()), Some("tailscale"));
    let f = stack("services:\n  web:\n    image: x\nnetworks:\n  wan: {}\n  vpn: {}\n");
    assert_eq!(f.overlay_network(&cluster()), Some("wan"));
    let f = stack("services:\n  web:\n    image: x\nnetworks:\n  vpn: {}\n");
    assert_eq!(f.overlay_network(&cluster()), None);
}

fn managed_cluster() -> ClusterConfig {
    let mut cluster = cluster();
    for name in ["mgd2", "mgd1"] {
        cluster.networks.insert(
            name.into(),
            serde_yaml::from_str("{ type: private, subnet: 192.168.32.0/24, managed: true }")
                .unwrap(),
        );
    }
    cluster.networks.get_mut("mgd2").unwrap().subnet = Some("192.168.33.0/24".parse().unwrap());
    let node = cluster.nodes.values_mut().next().unwrap();
    node.network.insert(
        "mgd1".into(),
        crate::cluster::NodeAddress::Ip("192.168.32.2".parse().unwrap()),
    );
    node.network.insert(
        "mgd2".into(),
        crate::cluster::NodeAddress::Ip("192.168.33.2".parse().unwrap()),
    );
    cluster
}

#[test]
fn managed_networks_are_never_the_stack_overlay() {
    let cluster = managed_cluster();
    let f = stack(
        "services:\n  web:\n    image: x\n    networks: [mgd2, lan, mgd1]\nnetworks:\n  mgd1: {}\n  mgd2: {}\n  lan: {}\n",
    );
    f.validate(&cluster).unwrap();
    assert_eq!(f.overlay_network(&cluster), Some("lan"));
    assert_eq!(f.primary_network(&cluster), Some("lan"));
    assert_eq!(f.managed_attachments(&cluster, "web"), vec!["mgd1", "mgd2"]);
    assert_eq!(f.managed_networks(&cluster), vec!["mgd1", "mgd2"]);
    assert!(f.managed_attachments(&cluster, "nope").is_empty());
}

#[test]
fn a_managed_only_stack_uses_its_first_managed_network_as_primary() {
    let cluster = managed_cluster();
    let f = stack(
        "services:\n  web:\n    image: x\n    networks: [mgd2, mgd1]\nnetworks:\n  mgd1: {}\n  mgd2: {}\n",
    );
    f.validate(&cluster).unwrap();
    assert_eq!(f.overlay_network(&cluster), None);
    assert_eq!(f.primary_network(&cluster), Some("mgd1"));
    // Public wins over nothing but a managed network still is not an overlay.
    let f = stack("services:\n  web:\n    image: x\nnetworks:\n  mgd1: {}\n  wan: {}\n");
    assert_eq!(f.overlay_network(&cluster), Some("wan"));
}

#[test]
fn a_managed_network_needs_no_node_address_to_host_a_stack() {
    let mut cluster = managed_cluster();
    let node = cluster.nodes.values_mut().next().unwrap();
    node.network
        .insert("mgd1".into(), crate::cluster::NodeAddress::None);
    let f = stack("services:\n  web:\n    image: x\n    networks: [mgd1]\nnetworks:\n  mgd1: {}\n");
    f.validate(&cluster)
        .expect("an address-less listing still hosts on a managed network");
    // An unmanaged private network does not: the listing is access only.
    let node = cluster.nodes.values_mut().next().unwrap();
    node.network
        .insert("lan".into(), crate::cluster::NodeAddress::None);
    for n in cluster.nodes.values_mut().skip(1) {
        n.network.remove("lan");
    }
    let node = cluster.nodes.values().next().unwrap();
    assert!(node.hosts_on("mgd1", true));
    assert!(!node.hosts_on("lan", false));
    assert!(!node.hosts_on("absent", true));
}

#[test]
fn a_managed_network_nobody_implements_is_refused() {
    let mut cluster = managed_cluster();
    for node in cluster.nodes.values_mut() {
        node.network.remove("mgd1");
    }
    let f = stack("services:\n  web:\n    image: x\n    networks: [mgd1]\nnetworks:\n  mgd1: {}\n");
    let err = f.validate(&cluster).unwrap_err().to_string();
    assert!(err.contains("no node implements network mgd1"), "{err}");
}

fn container_cluster(subnet: &str) -> ClusterConfig {
    let yaml = CLUSTER_YAML.replace(
        "  vpn:\n    type: egress\n",
        &format!(
            "  vpn:\n    type: egress\n  ctn2:\n    type: container\n    subnet: {subnet}\n    container: {{ stack: rt, service: router }}\n"
        ),
    );
    ClusterConfig::parse(&yaml).expect("patched cluster is valid")
}

#[test]
fn a_container_network_needs_no_implementing_node() {
    let raw = "networks:\n  ctn2: {}\nservices:\n  a:\n    image: x\n    networks: [ctn2]\n    x-clouduler: { node: n1 }\n";
    let cluster = container_cluster("100.64.0.0/10");
    let s = stack(raw);
    s.validate(&cluster).expect("every node implements it");
    assert_eq!(
        s.container_networks("a", &cluster),
        vec!["ctn2".to_string()]
    );
    assert!(s.container_networks("missing", &cluster).is_empty());
    // Never an overlay, never a pinning egress network.
    assert_eq!(s.overlay_network(&cluster), None);
    assert_eq!(s.egress_network("a", &cluster), None);
}

#[test]
fn only_a_default_route_container_network_counts_as_an_egress_network() {
    let raw = "networks:\n  vpn: {}\n  ctn2: {}\nservices:\n  a:\n    image: x\n    networks: [vpn, ctn2]\n";
    stack(raw)
        .validate(&container_cluster("100.64.0.0/10"))
        .expect("a non-default container network is not an egress network");
    let err = stack(raw)
        .validate(&container_cluster("0.0.0.0/0"))
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("only one egress network is allowed per service"));
}

#[test]
fn the_router_is_attached_automatically_and_runs_one_replica() {
    let cluster = container_cluster("100.64.0.0/10");
    let own = "networks:\n  ctn2: {}\nservices:\n  router:\n    image: x\n    networks: [ctn2]\n";
    let err = stack(own).validate_named("rt", &cluster).unwrap_err();
    assert!(err.to_string().contains("attached to it automatically"));
    // The same service in a different stack is an ordinary client.
    stack(own).validate_named("other", &cluster).unwrap();

    let ok = "services:\n  router:\n    image: x\n";
    stack(ok).validate_named("rt", &cluster).unwrap();
    for deploy in ["replicas: 2", "mode: global"] {
        let raw = format!("services:\n  router:\n    image: x\n    deploy:\n      {deploy}\n");
        let err = stack(&raw).validate_named("rt", &cluster).unwrap_err();
        assert!(err.to_string().contains("exactly one replica"), "{deploy}");
        stack(&raw).validate_named("other", &cluster).unwrap();
    }
}

#[test]
fn a_container_network_takes_no_allowed_list() {
    let raw = "networks:\n  ctn2:\n    allowed: [example.com]\nservices:\n  a:\n    image: x\n    networks: [ctn2]\n";
    let err = stack(raw)
        .validate(&container_cluster("100.64.0.0/10"))
        .unwrap_err();
    assert!(err.to_string().contains("container network"), "{err}");
}

#[test]
fn a_node_pin_is_fine_on_a_default_route_container_network() {
    let raw = "networks:\n  ctn2: {}\nservices:\n  a:\n    image: x\n    networks: [ctn2]\n    x-clouduler: { node: n1 }\n";
    stack(raw)
        .validate(&container_cluster("0.0.0.0/0"))
        .expect("every node implements it");
}

#[test]
fn a_pinned_subnet_is_validated() {
    let cluster = container_cluster("100.64.0.0/10");
    let ok = "x-clouduler:\n  subnet: 172.30.5.0/24\nservices:\n  a:\n    image: x\n    networks: [tailscale]\nnetworks:\n  tailscale: {}\n";
    stack(ok).validate(&cluster).expect("a free subnet is fine");
    for (subnet, want) in [
        ("172.30.5.7/24", "host bits"),
        ("172.30.5.0/30", "between /8 and /29"),
        ("10.10.0.0/25", "overlaps"),
    ] {
        let raw = ok.replace("172.30.5.0/24", subnet);
        let err = stack(&raw).validate(&cluster).unwrap_err();
        assert!(err.to_string().contains(want), "{subnet}: {err}");
    }
    let none = "x-clouduler:\n  subnet: 172.30.5.0/24\nservices:\n  a:\n    image: x\n";
    let err = stack(none).validate(&cluster).unwrap_err();
    assert!(err.to_string().contains("no overlay network"), "{err}");
}
