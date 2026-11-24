use super::*;

#[test]
fn a_fence_that_may_outlast_node_timeout_is_warned_about() {
    use crate::cluster::FailoverConfig;
    use std::time::Duration;
    let f = |fence, node, grace| FailoverConfig {
        fence_timeout: Duration::from_secs(fence),
        node_timeout: Duration::from_secs(node),
        probe_interval: Duration::from_secs(5),
        stop_grace: Duration::from_secs(grace),
        shutdown_push_timeout: Duration::from_secs(900),
    };
    assert!(f(30, 60, 30).fence_margin_warning().is_some());
    assert!(f(29, 60, 30).fence_margin_warning().is_none());
    // The configured grace is what counts, not a fixed default.
    assert!(f(29, 60, 31).fence_margin_warning().is_some());
    assert!(f(29, 60, 10).fence_margin_warning().is_none());
    // The defaults (fence 15s + grace 30s inside node 60s) are sane.
    assert!(FailoverConfig::default().fence_margin_warning().is_none());
    assert!(ClusterConfig::parse(BASE)
        .unwrap()
        .failover
        .fence_margin_warning()
        .is_none());
    assert!(f(10, 60, 10)
        .fence_margin_warning_for(Duration::from_secs(50), "service")
        .is_some());
}

#[test]
fn shutdown_push_timeout_defaults_and_is_bounded() {
    let cfg = ClusterConfig::parse(BASE).expect("valid");
    assert_eq!(
        cfg.failover.shutdown_push_timeout,
        std::time::Duration::from_secs(900)
    );
    let with = |value: &str| {
        parse(&[(
            "web: { address: cloud, network: tailscale }",
            &format!("failover: {{ shutdown_push_timeout: {value} }}\nweb: {{ address: cloud, network: tailscale }}"),
        )])
    };
    assert_eq!(
        with("0s")
            .expect("zero skips the push")
            .failover
            .shutdown_push_timeout,
        std::time::Duration::ZERO
    );
    assert!(with("30m").is_ok());
    let err = with("3h").unwrap_err();
    assert!(err.to_string().contains("failover.shutdown_push_timeout"));
}

#[test]
fn stop_grace_defaults_and_is_configurable() {
    let cfg = ClusterConfig::parse(BASE).expect("valid");
    assert_eq!(cfg.failover.stop_grace, std::time::Duration::from_secs(30));
    let cfg = parse(&[(
        "web: { address: cloud, network: tailscale }",
        "failover: { stop_grace: 2m }\nweb: { address: cloud, network: tailscale }",
    )])
    .expect("valid");
    assert_eq!(cfg.failover.stop_grace, std::time::Duration::from_secs(120));
}

#[test]
fn stop_grace_must_be_sane() {
    for bad in ["0s", "11m"] {
        let err = parse(&[(
            "web: { address: cloud, network: tailscale }",
            &format!(
                "failover: {{ stop_grace: {bad} }}\nweb: {{ address: cloud, network: tailscale }}"
            ),
        )])
        .unwrap_err();
        assert!(err.to_string().contains("failover.stop_grace"), "{bad}");
    }
}

#[test]
fn failover_defaults_are_ordered_correctly() {
    let cfg = ClusterConfig::parse(BASE).expect("valid");
    let f = &cfg.failover;
    assert!(f.probe_interval < f.fence_timeout);
    assert!(f.fence_timeout < f.node_timeout);
}

#[test]
fn fencing_must_beat_rescheduling() {
    let err = parse(&[(
        "web: { address: cloud, network: tailscale }",
        "failover: { node_timeout: 10s, fence_timeout: 20s }\nweb: { address: cloud, network: tailscale }",
    )])
    .unwrap_err();
    assert!(err
        .to_string()
        .contains("must be shorter than node_timeout"));
}

#[test]
fn probing_must_beat_fencing() {
    let err = parse(&[(
        "web: { address: cloud, network: tailscale }",
        "failover: { fence_timeout: 5s, probe_interval: 5s }\nweb: { address: cloud, network: tailscale }",
    )])
    .unwrap_err();
    assert!(err
        .to_string()
        .contains("must be shorter than fence_timeout"));
}

#[test]
fn failover_timings_are_configurable() {
    let cfg = parse(&[(
        "web: { address: cloud, network: tailscale }",
        "failover: { node_timeout: 2m, fence_timeout: 45s, probe_interval: 10s }\nweb: { address: cloud, network: tailscale }",
    )])
    .expect("valid");
    assert_eq!(
        cfg.failover.node_timeout,
        std::time::Duration::from_secs(120)
    );
    assert_eq!(
        cfg.failover.fence_timeout,
        std::time::Duration::from_secs(45)
    );
}
