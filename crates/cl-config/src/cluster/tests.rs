use super::*;

#[test]
fn a_trailing_dot_is_stripped_and_used_verbatim() {
    assert_eq!(
        qualify("app.example.net.", Some("cluster.lan")),
        "app.example.net"
    );
}

#[test]
fn a_trailing_dot_wins_over_a_bare_label() {
    assert_eq!(qualify("blog.", Some("cluster.lan")), "blog");
    assert_eq!(qualify("Blog", Some("Cluster.LAN")), "blog.cluster.lan");
    assert_eq!(qualify("App.Example.NET.", None), "app.example.net");
}

#[test]
fn mcp_is_on_unless_a_config_turns_it_off() {
    assert!(McpConfig::default().enabled);
    assert!(serde_yaml::from_str::<McpConfig>("{}").unwrap().enabled);
    let off: McpConfig = serde_yaml::from_str("enabled: false").unwrap();
    assert!(!off.enabled);
    let round: McpConfig = serde_yaml::from_str(&serde_yaml::to_string(&off).unwrap()).unwrap();
    assert_eq!(round, off);
}

#[test]
fn at_sign_names_the_apex_of_the_domain() {
    assert_eq!(qualify("@", Some("Cluster.lan")), "cluster.lan");
}

#[test]
fn without_a_trailing_dot_a_bare_label_still_gets_the_domain() {
    assert_eq!(qualify("blog", Some("cluster.lan")), "blog.cluster.lan");
}
