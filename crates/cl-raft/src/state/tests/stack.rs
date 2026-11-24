use super::*;

#[test]
fn version_advances_only_on_accepted_commands() {
    let mut state = cluster();
    let before = state.version;
    let rejected = assign(&mut state, "nowhere", &[]);
    assert!(!rejected.is_ok());
    assert_eq!(state.version, before);

    let accepted = assign(&mut state, "n1", &[]);
    assert!(accepted.is_ok());
    assert_eq!(state.version, before + 1);
}

#[test]
fn repinning_remembers_the_previous_digest_unless_it_is_unchanged() {
    let mut state = cluster();
    assert!(assign(&mut state, "n1", &[]).is_ok());
    let instance = InstanceId::new("gitlab", "web", 0);
    let pin = |state: &mut ClusterState, digest: &str| {
        state.apply(&Request::SetImageDigest {
            instance: instance.clone(),
            digest: digest.to_string(),
        })
    };
    pin(&mut state, "sha256:a");
    assert!(!state.previous_image_digests.contains_key(&instance));
    pin(&mut state, "sha256:b");
    assert_eq!(state.previous_image_digests[&instance], "sha256:a");
    pin(&mut state, "sha256:b");
    assert_eq!(state.previous_image_digests[&instance], "sha256:a");
    state.apply(&Request::Unassign {
        instance: instance.clone(),
    });
    assert!(state.previous_image_digests.is_empty());
}

#[test]
fn reassigning_an_instance_to_a_new_node_keeps_its_image_pin() {
    let mut state = cluster();
    assert!(assign(&mut state, "n1", &[]).is_ok());

    let instance = InstanceId::new("gitlab", "web", 0);
    state.apply(&Request::SetImageDigest {
        instance: instance.clone(),
        digest: "sha256:pinned".to_string(),
    });
    assert_eq!(
        state.assignments[&instance].image_digest.as_deref(),
        Some("sha256:pinned")
    );

    // The scheduler moves the instance to a different node (its old one
    // went unreachable, say) by submitting `Assign` again, with no
    // intervening `Unassign`. The pin must survive that move.
    assert!(assign(&mut state, "n2", &[]).is_ok());
    assert_eq!(state.assignments[&instance].node, "n2");
    assert_eq!(
        state.assignments[&instance].image_digest.as_deref(),
        Some("sha256:pinned")
    );
}

#[test]
fn placement_requires_every_drive() {
    let mut state = cluster();
    assert!(assign(&mut state, "n1", &["fast", "bulk"]).is_ok());

    let mut state = cluster();
    let rejected = assign(&mut state, "n2", &["fast", "bulk"]);
    assert!(rejected
        .error
        .unwrap()
        .contains("n2 does not implement drive bulk"));
}

/// A served drive has one authoritative copy reachable by any node, so
/// `check_placement` must not reject a node lacking a local directory for it;
/// must agree with `cl_sched::place::rejection`.
#[test]
fn a_served_drive_does_not_require_the_node_to_implement_it() {
    let served_config = Request::PutClusterConfig {
        yaml: "drives:\n  bulk:\n    mode: served\n    served_by: [n1]\n".into(),
        expect_generation: 0,
    };

    // n3 implements neither "fast" nor "bulk" (see `cluster()`), but
    // "bulk" is now served, so only "fast" should still block it.
    let mut state = cluster();
    state.apply(&served_config);
    assert!(assign(&mut state, "n3", &["bulk"]).is_ok());

    let mut state = cluster();
    state.apply(&served_config);
    let rejected = assign(&mut state, "n3", &["fast"]);
    assert!(rejected
        .error
        .unwrap()
        .contains("n3 does not implement drive fast"));
}

#[test]
fn redeploying_a_stack_bumps_the_revision() {
    let mut state = ClusterState::default();
    state.apply(&Request::DeployStack {
        name: "gitlab".into(),
        spec: "a".into(),
        at: 0,
    });
    assert_eq!(state.stacks["gitlab"].revision, 1);
    state.apply(&Request::DeployStack {
        name: "gitlab".into(),
        spec: "b".into(),
        at: 0,
    });
    assert_eq!(state.stacks["gitlab"].revision, 2);
    assert_eq!(state.stacks["gitlab"].spec, "b");
}

#[test]
fn removing_a_stack_drops_its_assignments() {
    let mut state = cluster();
    assign(&mut state, "n1", &[]);
    state.apply(&Request::RemoveStack {
        name: "gitlab".into(),
    });
    assert!(state.assignments.is_empty());
}

#[test]
fn prioritizing_needs_a_deployed_stack_and_goes_away_with_it() {
    let mut state = cluster();
    let on = |stack: &str| Request::SetPrioritized {
        stack: stack.into(),
        on: true,
    };
    assert!(state.apply(&on("site")).error.is_some());
    state.apply(&Request::DeployStack {
        name: "site".into(),
        spec: "services:\n  web:\n    image: nginx\n".into(),
        at: 0,
    });
    assert!(state.apply(&on("site")).error.is_none());
    assert_eq!(state.effective_priority("site", 5), i32::MIN);
    assert_eq!(state.effective_priority("other", 5), 5);
    state.apply(&Request::SetPrioritized {
        stack: "site".into(),
        on: false,
    });
    assert!(state.prioritized.is_empty());
    state.apply(&on("site"));
    state.apply(&Request::RemoveStack {
        name: "site".into(),
    });
    assert!(state.prioritized.is_empty());
}

#[test]
fn assignments_on_filters_by_node() {
    let mut state = cluster();
    assign(&mut state, "n1", &[]);
    state.apply(&Request::Assign {
        instance: InstanceId::new("gitlab", "web", 1),
        node: "n2".into(),
        drives: drives(&["fast"]),
    });
    assert_eq!(state.assignments_on("n1").count(), 1);
    assert_eq!(state.assignments_on("n2").count(), 1);
    assert_eq!(state.assignments_on("n3").count(), 0);
}

#[test]
fn reassigning_keeps_an_operator_stop_and_drops_a_stale_migration() {
    let mut state = cluster();
    let instance = InstanceId::new("gitlab", "web", 0);
    assert!(assign(&mut state, "n1", &[]).is_ok());
    state.apply(&Request::SetDesiredState {
        instance: instance.clone(),
        desired: DesiredState::Stopped,
    });
    assert!(assign(&mut state, "n2", &[]).is_ok());
    assert_eq!(state.assignments[&instance].node, "n2");
    assert_eq!(state.assignments[&instance].desired, DesiredState::Stopped);

    // A migration in flight survives a same-node re-Assign but not a move.
    assert!(state
        .apply(&Request::BeginMigration {
            instance: instance.clone(),
            to: "n1".into(),
            drives: Default::default(),
        })
        .is_ok());
    assert!(assign(&mut state, "n2", &[]).is_ok());
    assert!(state.assignments[&instance].migration.is_some());
    assert!(assign(&mut state, "n3", &[]).is_ok());
    assert!(state.assignments[&instance].migration.is_none());
}

#[test]
fn a_container_report_carries_secret_revisions_and_a_bare_one_clears_them() {
    let mut state = ClusterState::default();
    let id = InstanceId::new("app", "web", 0);
    state.apply(&Request::ReportContainerStatus {
        node: "n1".into(),
        running: BTreeMap::from([(id.clone(), true)]),
        secret_revisions: BTreeMap::from([(id.clone(), BTreeMap::from([("db".to_string(), 2)]))]),
        config_revisions: BTreeMap::new(),
    });
    assert_eq!(state.container_status[&id].secret_revisions["db"], 2);

    // A report from a node that predates the field omits it.
    let request: Request = serde_json::from_str(
        r#"{"ReportContainerStatus":{"node":"n1","running":{"app.web.0":true}}}"#,
    )
    .expect("old-shape report");
    state.apply(&request);
    assert!(state.container_status[&id].secret_revisions.is_empty());
}
