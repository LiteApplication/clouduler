use super::*;

#[test]
fn saving_addresses_is_opt_in_per_network() {
    let base = format!("{BASE}{AUTH}");
    let cfg = ClusterConfig::parse(&base).unwrap();
    let auth = cfg.proxy.auth.unwrap();
    assert!(!auth.remembers_ip("lan") && !auth.remembers_ip("wan"));

    let on = format!("{base}    remember_ip: [wan]\n");
    let auth = ClusterConfig::parse(&on).unwrap().proxy.auth.unwrap();
    assert!(auth.remembers_ip("wan") && !auth.remembers_ip("lan"));
    assert_eq!(
        auth.remember_ip_ttl,
        std::time::Duration::from_secs(30 * 86_400)
    );
}

#[test]
fn a_network_without_a_login_cannot_save_addresses() {
    let bad = format!("{BASE}{AUTH}    remember_ip: [tailscale]\n");
    let err = ClusterConfig::parse(&bad).unwrap_err().to_string();
    assert!(err.contains("remember_ip"), "{err}");
    let zero = format!("{BASE}{AUTH}    remember_ip_ttl: 0s\n");
    assert!(ClusterConfig::parse(&zero).is_err());
}

#[test]
fn api_keys_cannot_be_the_only_way_in_and_disabled() {
    let err = parse(&[(
        "web: { address: cloud, network: tailscale }",
        "auth: { api_keys: false }\nweb: { address: cloud, network: tailscale }",
    )])
    .unwrap_err();
    assert!(err.to_string().contains("lock everyone out"));
}

#[test]
fn oidc_is_validated_when_present() {
    let good = "auth:\n  api_keys: false\n  oidc:\n    issuer: https://accounts.example.com\n    client_id: clouduler\n";
    let cfg = parse(&[(
        "web: { address: cloud, network: tailscale }",
        &format!("{good}web: {{ address: cloud, network: tailscale }}"),
    )])
    .expect("valid");
    let oidc = cfg.auth.oidc.expect("oidc");
    assert_eq!(oidc.scopes, vec!["openid", "profile", "email"]);
    assert_eq!(oidc.admin_claim, "groups");

    let bad = good.replace("https://accounts", "http://accounts");
    let err = parse(&[(
        "web: { address: cloud, network: tailscale }",
        &format!("{bad}web: {{ address: cloud, network: tailscale }}"),
    )])
    .unwrap_err();
    assert!(err.to_string().contains("https url"));
}

#[test]
fn secrets_default_to_a_generated_key() {
    let cfg = ClusterConfig::parse(BASE).expect("valid");
    assert!(cfg.secrets.generate_if_missing);
    assert_eq!(
        cfg.secrets.key_file,
        std::path::PathBuf::from("/etc/clouduler/secrets.key")
    );
}
