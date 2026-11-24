use super::*;

#[test]
fn the_dns_server_is_off_by_default_and_checks_its_networks() {
    let cfg = ClusterConfig::parse(BASE).unwrap();
    assert!(!cfg.proxy.dns_server.enabled);
    assert_eq!(cfg.proxy.dns_server.port, 53);

    let on = format!("{BASE}proxy:\n  dns_server: {{ enabled: true, networks: [lan, wan] }}\n");
    let cfg = ClusterConfig::parse(&on).unwrap();
    assert_eq!(cfg.proxy.dns_server.networks, ["lan", "wan"]);

    let unknown = format!("{BASE}proxy:\n  dns_server: {{ networks: [nope] }}\n");
    assert!(ClusterConfig::parse(&unknown).is_err());
    let port = format!("{BASE}proxy:\n  dns_server: {{ port: 0 }}\n");
    assert!(ClusterConfig::parse(&port).is_err());
}

#[test]
fn the_dns_server_upstream_is_a_list_of_resolver_addresses() {
    let cfg = ClusterConfig::parse(BASE).unwrap();
    assert!(cfg.proxy.dns_server.upstream.is_empty());

    let ok =
        format!("{BASE}proxy:\n  dns_server: {{ upstream: [9.9.9.9, '2606:4700:4700::1111'] }}\n");
    let cfg = ClusterConfig::parse(&ok).unwrap();
    assert_eq!(cfg.proxy.dns_server.upstream.len(), 2);

    for (list, needle) in [
        ("[0.0.0.0]", "not a resolver"),
        ("[127.0.0.1]", "not a resolver"),
        ("[224.0.0.1]", "not a resolver"),
        ("[1.1.1.1, 1.1.1.1]", "twice"),
    ] {
        let bad = format!("{BASE}proxy:\n  dns_server: {{ upstream: {list} }}\n");
        let err = ClusterConfig::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("proxy.dns_server.upstream"), "{err}");
        assert!(err.contains(needle), "{err}");
    }
    let name = format!("{BASE}proxy:\n  dns_server: {{ upstream: [dns.google] }}\n");
    assert!(ClusterConfig::parse(&name).is_err());
}
