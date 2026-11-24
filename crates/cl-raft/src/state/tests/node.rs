use super::*;

#[test]
fn holds_image_matches_a_pinned_digest_or_a_tag() {
    let mut n = node("n1", &[]);
    n.images.insert("nginx:1.25".into());
    n.images.insert("ghcr.io/o/app@sha256:abc".into());
    assert!(n.holds_image("nginx:1.25", None));
    assert!(!n.holds_image("nginx:1.26", None));
    assert!(n.holds_image("ghcr.io/o/app:v2", Some("sha256:abc")));
    assert!(!n.holds_image("ghcr.io/o/app:v2", Some("sha256:def")));
    assert!(!node("n2", &[]).holds_image("nginx:1.25", None));
}

#[test]
fn drained_nodes_take_no_new_work() {
    let mut state = cluster();
    state.apply(&Request::SetAvailability {
        node: "n1".into(),
        availability: Availability::Drain,
    });
    let rejected = assign(&mut state, "n1", &[]);
    assert!(rejected.error.unwrap().contains("Drain"));
}

#[test]
fn an_unreachable_node_takes_no_new_work() {
    let mut state = cluster();
    state.apply(&Request::SetReachable {
        node: "n1".into(),
        reachable: false,
    });
    assert!(!state.can_schedule("n1"));

    let rejected = assign(&mut state, "n1", &[]);
    assert!(rejected.error.unwrap().contains("n1 is unreachable"));

    // The node coming back makes it a candidate again.
    state.apply(&Request::SetReachable {
        node: "n1".into(),
        reachable: true,
    });
    assert!(state.can_schedule("n1"));
    assert!(assign(&mut state, "n1", &[]).is_ok());
}

#[test]
fn reachability_is_only_tracked_for_known_nodes() {
    let mut state = cluster();
    let rejected = state.apply(&Request::SetReachable {
        node: "ghost".into(),
        reachable: false,
    });
    assert!(rejected.error.unwrap().contains("unknown node ghost"));
}

#[test]
fn removing_a_node_clears_its_unreachable_flag() {
    let mut state = cluster();
    state.apply(&Request::SetReachable {
        node: "n1".into(),
        reachable: false,
    });
    state.apply(&Request::RemoveNode { node: "n1".into() });
    assert!(state.unreachable.is_empty());
}

#[test]
fn nodes_running_work_cannot_be_removed() {
    let mut state = cluster();
    assign(&mut state, "n1", &[]);
    let rejected = state.apply(&Request::RemoveNode { node: "n1".into() });
    assert!(rejected.error.unwrap().contains("still runs gitlab.web.0"));

    state.apply(&Request::Unassign {
        instance: InstanceId::new("gitlab", "web", 0),
    });
    assert!(state
        .apply(&Request::RemoveNode { node: "n1".into() })
        .is_ok());
    assert!(!state.nodes.contains_key("n1"));
}

#[test]
fn remove_node_reassigns_volume_primary_and_purges_container_status() {
    let mut state = cluster();
    let id = VolumeId::new("fast", "data");
    assert!(state
        .apply(&Request::UpsertVolume {
            id: id.clone(),
            primary: "n1".into(),
        })
        .is_ok());
    assert!(state
        .apply(&Request::SetVolumeReplica {
            id: id.clone(),
            node: "n2".into(),
            generation: 1,
        })
        .is_ok());
    state.apply(&Request::ReportContainerStatus {
        node: "n1".into(),
        running: BTreeMap::from([(InstanceId::new("gitlab", "web", 0), true)]),
        secret_revisions: BTreeMap::new(),
        config_revisions: BTreeMap::new(),
    });
    assert!(!state.container_status.is_empty());

    assert!(state
        .apply(&Request::RemoveNode { node: "n1".into() })
        .is_ok());
    assert_eq!(state.volumes[&id].primary, "n2");
    assert!(!state.volumes[&id].replicas.contains_key("n1"));
    assert!(state.container_status.is_empty());
}

#[test]
fn remove_node_with_no_other_replica_still_succeeds() {
    let mut state = cluster();
    let id = VolumeId::new("fast", "data");
    state.apply(&Request::UpsertVolume {
        id: id.clone(),
        primary: "n1".into(),
    });
    assert!(state
        .apply(&Request::RemoveNode { node: "n1".into() })
        .is_ok());
    assert!(state.volumes[&id].replicas.is_empty());
}
