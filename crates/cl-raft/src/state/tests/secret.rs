use super::*;

#[test]
fn the_first_cluster_config_is_stored_at_generation_one() {
    let mut state = ClusterState::default();
    let response = state.apply(&Request::PutClusterConfig {
        yaml: "cluster:\n  name: home\n".into(),
        expect_generation: 0,
    });

    assert!(response.is_ok(), "{:?}", response.error);
    let stored = state.cluster_config.expect("stored");
    assert_eq!(stored.generation, 1);
    assert!(stored.yaml.contains("home"));
}

#[test]
fn a_config_edit_made_against_a_stale_generation_is_refused() {
    let mut state = ClusterState::default();
    state.apply(&Request::PutClusterConfig {
        yaml: "first".into(),
        expect_generation: 0,
    });

    // Two admins both read generation 1 and both save. The second one has
    // not seen the first one's change, so it must not land.
    let first = state.apply(&Request::PutClusterConfig {
        yaml: "second".into(),
        expect_generation: 1,
    });
    let second = state.apply(&Request::PutClusterConfig {
        yaml: "third".into(),
        expect_generation: 1,
    });

    assert!(first.is_ok());
    assert!(second.error.is_some());
    let stored = state.cluster_config.expect("stored");
    assert_eq!(stored.yaml, "second");
    assert_eq!(stored.generation, 2);
}

#[test]
fn put_config_cas_rejects_a_stale_revision() {
    let mut state = cluster();
    let put = |state: &mut ClusterState, body: &[u8], expect: u64| {
        state.apply(&Request::PutConfigCas {
            name: "run".into(),
            content: base64_encode(body),
            owner: None,
            expect_revision: expect,
        })
    };
    assert!(put(&mut state, b"one", 0).is_ok());
    assert_eq!(state.configs["run"].revision, 1);
    // A writer that read revision 1 wins; one that also read 1 loses.
    assert!(put(&mut state, b"two", 1).is_ok());
    let stale = put(&mut state, b"three", 1);
    assert!(!stale.is_ok());
    assert_eq!(state.configs["run"].decode().expect("decode"), b"two");
    // Creating over an existing object is a conflict too.
    assert!(!put(&mut state, b"four", 0).is_ok());
}

#[test]
fn put_wrapped_key_requires_a_known_node() {
    let mut state = cluster();
    let rejected = state.apply(&Request::PutWrappedKey {
        node: "nowhere".into(),
        entry: wrapped_key_entry("pub"),
    });
    assert!(rejected.error.unwrap().contains("unknown node nowhere"));
}

#[test]
fn put_wrapped_key_requires_a_matching_published_pubkey() {
    let mut state = cluster();
    // n1 has not published a pubkey yet.
    let rejected = state.apply(&Request::PutWrappedKey {
        node: "n1".into(),
        entry: wrapped_key_entry("pub"),
    });
    assert!(rejected.error.is_some());
    assert!(state.wrapped_keys.is_empty());
}

#[test]
fn put_wrapped_key_accepts_and_overwrites() {
    let mut state = cluster();
    let mut n1 = state.nodes.get("n1").unwrap().clone();
    n1.node_pubkey = Some("pub-a".into());
    state.apply(&Request::UpsertNode(n1));

    assert!(state
        .apply(&Request::PutWrappedKey {
            node: "n1".into(),
            entry: wrapped_key_entry("pub-a"),
        })
        .is_ok());
    assert_eq!(state.wrapped_keys["n1"].target_pubkey, "pub-a");

    // A rotated keypair: the old wrapped entry no longer matches.
    let mut n1 = state.nodes.get("n1").unwrap().clone();
    n1.node_pubkey = Some("pub-b".into());
    state.apply(&Request::UpsertNode(n1));
    let rejected = state.apply(&Request::PutWrappedKey {
        node: "n1".into(),
        entry: wrapped_key_entry("pub-a"),
    });
    assert!(rejected.error.is_some());

    assert!(state
        .apply(&Request::PutWrappedKey {
            node: "n1".into(),
            entry: wrapped_key_entry("pub-b"),
        })
        .is_ok());
    assert_eq!(state.wrapped_keys["n1"].target_pubkey, "pub-b");
}

#[test]
fn removing_a_node_clears_its_wrapped_key() {
    let mut state = cluster();
    let mut n1 = state.nodes.get("n1").unwrap().clone();
    n1.node_pubkey = Some("pub-a".into());
    state.apply(&Request::UpsertNode(n1));
    state.apply(&Request::PutWrappedKey {
        node: "n1".into(),
        entry: wrapped_key_entry("pub-a"),
    });
    assert!(!state.wrapped_keys.is_empty());

    state.apply(&Request::RemoveNode { node: "n1".into() });
    assert!(!state.wrapped_keys.contains_key("n1"));
}

#[test]
fn configs_are_stored_and_versioned() {
    let mut state = ClusterState::default();
    assert!(state
        .apply(&Request::PutConfig {
            name: "nginx_conf".into(),
            content: base64_encode(b"server {}"),
            owner: None,
        })
        .is_ok());
    assert_eq!(state.configs["nginx_conf"].revision, 1);
    assert_eq!(state.configs["nginx_conf"].decode().unwrap(), b"server {}");

    state.apply(&Request::PutConfig {
        name: "nginx_conf".into(),
        content: base64_encode(b"server { listen 80; }"),
        owner: None,
    });
    assert_eq!(state.configs["nginx_conf"].revision, 2);
}

#[test]
fn secrets_are_stored_sealed_and_redacted() {
    let mut state = ClusterState::default();
    state.apply(&Request::PutSecret {
        name: "db_password".into(),
        sealed: base64_encode(b"ciphertext-goes-here"),
        key_fingerprint: "abc123".into(),
        owner: None,
    });

    assert_eq!(state.secrets["db_password"].key_fingerprint, "abc123");
    // Nothing about the value leaves the node.
    assert!(state.redacted().secrets["db_password"].sealed.is_empty());
    // Nor reaches a log line.
    let rendered = format!("{:?}", state.secrets["db_password"]);
    assert!(!rendered.contains("ciphertext"));
}

/// The self-update deploy key rides the ordinary secrets map under a reserved name; `redacted()` needs no special case.
#[test]
fn redacted_blanks_the_self_update_deploy_key_like_any_other_secret() {
    let mut state = ClusterState::default();
    state.apply(&Request::PutSecret {
        name: "_clouduler_self_update_deploy_key".into(),
        sealed: base64_encode(b"ssh-private-key-ciphertext"),
        key_fingerprint: "abc123".into(),
        owner: None,
    });
    assert!(
        state.redacted().secrets["_clouduler_self_update_deploy_key"]
            .sealed
            .is_empty()
    );
}

#[test]
fn redacted_drops_login_sessions_and_the_addresses_saved_in_them() {
    let mut state = ClusterState::default();
    state.apply(&Request::PutProxySession {
        key: "k".into(),
        session: ProxySessionRecord {
            host: format!("{REMEMBERED_IP_HOST_PREFIX}public:203.0.113.7"),
            subject: "s".into(),
            username: "alice".into(),
            groups: vec![],
            expires_at: i64::MAX / 4,
            parent: String::new(),
        },
        now: 0,
    });
    assert_eq!(state.proxy_sessions.len(), 1);
    assert!(state.redacted().proxy_sessions.is_empty());
}

#[test]
fn object_names_must_be_safe_as_filenames() {
    let mut state = ClusterState::default();
    for bad in ["../escape", "with/slash", ".hidden", ""] {
        let rejected = state.apply(&Request::PutConfig {
            name: bad.into(),
            content: base64_encode(b"x"),
            owner: None,
        });
        assert!(!rejected.is_ok(), "{bad} should be refused");
    }
    assert!(state.configs.is_empty());
}

#[test]
fn objects_in_use_cannot_be_removed() {
    let mut state = ClusterState::default();
    state.apply(&Request::DeployStack {
        name: "site".into(),
        spec: "services: {}".into(),
        at: 0,
    });
    state.apply(&Request::PutConfig {
        name: "nginx_conf".into(),
        content: base64_encode(b"x"),
        owner: Some("site".into()),
    });
    state.apply(&Request::SetStackObjects {
        name: "site".into(),
        configs: BTreeSet::from(["nginx_conf".to_string()]),
        secrets: BTreeSet::new(),
        used_configs: BTreeSet::new(),
        used_secrets: BTreeSet::new(),
    });

    let rejected = state.apply(&Request::RemoveConfig {
        name: "nginx_conf".into(),
    });
    assert!(rejected.error.unwrap().contains("still used by stack site"));
}

#[test]
fn removing_a_stack_takes_its_objects_with_it() {
    let mut state = ClusterState::default();
    state.apply(&Request::DeployStack {
        name: "site".into(),
        spec: "services: {}".into(),
        at: 0,
    });
    state.apply(&Request::PutConfig {
        name: "nginx_conf".into(),
        content: base64_encode(b"x"),
        owner: Some("site".into()),
    });
    state.apply(&Request::PutSecret {
        name: "db_password".into(),
        sealed: base64_encode(b"y"),
        key_fingerprint: "abc".into(),
        owner: Some("site".into()),
    });
    state.apply(&Request::SetStackObjects {
        name: "site".into(),
        configs: BTreeSet::from(["nginx_conf".to_string()]),
        secrets: BTreeSet::from(["db_password".to_string()]),
        used_configs: BTreeSet::new(),
        used_secrets: BTreeSet::new(),
    });

    state.apply(&Request::RemoveStack {
        name: "site".into(),
    });
    assert!(state.configs.is_empty());
    assert!(state.secrets.is_empty());
}

#[test]
fn redeploying_a_stack_keeps_its_object_ownership() {
    let mut state = ClusterState::default();
    state.apply(&Request::DeployStack {
        name: "site".into(),
        spec: "a".into(),
        at: 0,
    });
    state.apply(&Request::SetStackObjects {
        name: "site".into(),
        configs: BTreeSet::from(["c".to_string()]),
        secrets: BTreeSet::new(),
        used_configs: BTreeSet::new(),
        used_secrets: BTreeSet::new(),
    });
    state.apply(&Request::DeployStack {
        name: "site".into(),
        spec: "b".into(),
        at: 0,
    });
    assert_eq!(state.stacks["site"].configs.len(), 1);
}

#[test]
fn an_unused_object_can_be_removed() {
    let mut state = ClusterState::default();
    state.apply(&Request::PutConfig {
        name: "orphan".into(),
        content: base64_encode(b"x"),
        owner: None,
    });
    assert!(state
        .apply(&Request::RemoveConfig {
            name: "orphan".into()
        })
        .is_ok());
    assert!(state.configs.is_empty());
}
