use super::*;

#[test]
fn a_proxy_token_is_redeemed_once_for_its_own_host_only() {
    let mut state = ClusterState::default();
    let put = |state: &mut ClusterState, expires_at| {
        state.apply(&Request::PutProxyToken {
            key: "t".into(),
            token: ProxyTokenRecord {
                host: "a.example.com".into(),
                subject: "sub".into(),
                username: "alice".into(),
                groups: vec!["staff".into()],
                session_expires_at: 1000,
                expires_at,
                parent: String::new(),
            },
            now: 0,
        });
    };
    let redeem = |state: &mut ClusterState, host: &str, key: &str, now| {
        state.apply(&Request::RedeemProxyToken {
            token_key: "t".into(),
            host: host.into(),
            session_key: key.into(),
            now,
        })
    };

    put(&mut state, 60);
    assert!(redeem(&mut state, "b.example.com", "s1", 1).error.is_some());
    assert!(state.proxy_sessions.is_empty());
    assert!(state.proxy_tokens.contains_key("t"));

    assert!(redeem(&mut state, "a.example.com", "s1", 1).error.is_none());
    let session = &state.proxy_sessions["s1"];
    assert_eq!(session.host, "a.example.com");
    assert_eq!(session.groups, vec!["staff"]);
    assert_eq!(session.expires_at, 1000);
    // Spent: a second redeem fails and mints nothing.
    assert!(redeem(&mut state, "a.example.com", "s2", 2).error.is_some());
    assert!(!state.proxy_sessions.contains_key("s2"));

    // Expired tokens do not redeem.
    put(&mut state, 60);
    assert!(redeem(&mut state, "a.example.com", "s3", 60)
        .error
        .is_some());
    assert!(!state.proxy_sessions.contains_key("s3"));
}

#[test]
fn signing_out_ends_the_app_sessions_it_handed_out() {
    let mut state = ClusterState::default();
    let portal = ProxySessionRecord {
        host: "login.example.com".into(),
        subject: "sub".into(),
        username: "alice".into(),
        groups: Vec::new(),
        expires_at: 1000,
        parent: String::new(),
    };
    state.apply(&Request::PutProxySession {
        key: "portal".into(),
        session: portal,
        now: 0,
    });
    let token = |parent: &str| ProxyTokenRecord {
        host: "a.example.com".into(),
        subject: "sub".into(),
        username: "alice".into(),
        groups: Vec::new(),
        session_expires_at: 1000,
        expires_at: 60,
        parent: parent.into(),
    };
    for key in ["t1", "t2"] {
        state.apply(&Request::PutProxyToken {
            key: key.into(),
            token: token("portal"),
            now: 0,
        });
    }
    let redeem = |state: &mut ClusterState, t: &str, s: &str| {
        state.apply(&Request::RedeemProxyToken {
            token_key: t.into(),
            host: "a.example.com".into(),
            session_key: s.into(),
            now: 1,
        })
    };
    assert!(redeem(&mut state, "t1", "app").error.is_none());
    assert_eq!(state.proxy_sessions["app"].parent, "portal");

    state.apply(&Request::RemoveProxySession {
        key: "portal".into(),
    });
    assert!(state.proxy_sessions.is_empty());
    // A token minted before the sign-out is gone too, and a late one
    // naming the dead portal session cannot mint anything.
    assert!(state.proxy_tokens.is_empty());
    state.apply(&Request::PutProxyToken {
        key: "t3".into(),
        token: token("portal"),
        now: 2,
    });
    assert!(redeem(&mut state, "t3", "late").error.is_some());
    assert!(state.proxy_sessions.is_empty());
}

#[test]
fn sessions_are_stored_removed_and_pruned() {
    let mut state = ClusterState::default();
    let session = |expires_at| SessionRecord {
        subject: "s".into(),
        username: "u".into(),
        admin: true,
        expires_at,
    };
    state.apply(&Request::PutSession {
        key: "old".into(),
        session: session(10),
        now: 0,
    });
    state.apply(&Request::PutSession {
        key: "live".into(),
        session: session(100),
        now: 50,
    });
    // Storing a new session sweeps the one that expired at 10.
    assert!(!state.sessions.contains_key("old"));
    assert!(state.sessions.contains_key("live"));

    state.apply(&Request::PruneSessions { now: 100 });
    assert!(state.sessions.is_empty());

    state.apply(&Request::PutSession {
        key: "k".into(),
        session: session(100),
        now: 0,
    });
    state.apply(&Request::RemoveSession { key: "k".into() });
    assert!(state.sessions.is_empty());
}
