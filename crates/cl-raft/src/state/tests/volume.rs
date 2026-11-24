use super::*;

#[test]
fn volume_generation_marks_replicas_stale() {
    let mut state = cluster();
    let id = VolumeId::new("fast", "data");
    state.apply(&Request::UpsertVolume {
        id: id.clone(),
        primary: "n1".into(),
    });
    state.apply(&Request::SetVolumeReplica {
        id: id.clone(),
        node: "n2".into(),
        generation: 1,
    });
    assert_eq!(state.volumes[&id].in_sync(), ["n1", "n2"].into());

    state.apply(&Request::TouchVolume {
        id: id.clone(),
        primary: "n1".into(),
    });
    assert_eq!(state.volumes[&id].in_sync(), ["n1"].into());
    assert_eq!(state.volumes[&id].generation, 2);
}

#[test]
fn a_sync_refusal_is_recorded_and_cleared() {
    let mut state = cluster();
    let id = VolumeId::new("fast", "data");
    state.apply(&Request::UpsertVolume {
        id: id.clone(),
        primary: "n1".into(),
    });
    let refusal = SyncRefusal {
        from: "n1".into(),
        to: "n2".into(),
        reason: "source is empty".into(),
        at: 100,
    };
    let set = Request::SetVolumeSyncRefusal {
        id: id.clone(),
        refusal: Some(refusal.clone()),
    };
    let first = state.apply(&set);
    assert!(first.is_ok());
    assert_eq!(state.volumes[&id].sync_refused, Some(refusal));

    state.apply(&Request::SetVolumeSyncRefusal {
        id: id.clone(),
        refusal: None,
    });
    assert!(state.volumes[&id].sync_refused.is_none());

    let rejected = state.apply(&Request::SetVolumeSyncRefusal {
        id: VolumeId::new("fast", "nope"),
        refusal: None,
    });
    assert!(rejected.error.unwrap().contains("unknown volume"));
}

#[test]
fn freezing_a_volume_blocks_a_second_freeze_and_unfreeze_makes_the_node_primary() {
    let mut state = cluster();
    let id = VolumeId::new("fast", "data");
    state.apply(&Request::UpsertVolume {
        id: id.clone(),
        primary: "n1".into(),
    });
    state.apply(&Request::SetVolumeReplica {
        id: id.clone(),
        node: "n2".into(),
        generation: 1,
    });

    assert!(state
        .apply(&Request::FreezeVolume {
            id: id.clone(),
            node: "n2".into(),
            stopped: vec![],
        })
        .is_ok());
    assert!(state
        .apply(&Request::FreezeVolume {
            id: id.clone(),
            node: "n1".into(),
            stopped: vec![],
        })
        .error
        .unwrap()
        .contains("already frozen"));

    assert!(state
        .apply(&Request::UnfreezeVolume { id: id.clone() })
        .is_ok());
    let volume = &state.volumes[&id];
    assert!(volume.frozen.is_none());
    assert_eq!(volume.primary, "n2");
    assert_eq!(volume.generation, 2);
    assert_eq!(state.replicas_behind(&id), ["n1"]);
    assert!(state
        .apply(&Request::UnfreezeVolume { id })
        .error
        .unwrap()
        .contains("not frozen"));
}

#[test]
fn replica_progress_never_goes_backwards() {
    let mut state = cluster();
    let id = VolumeId::new("fast", "data");
    state.apply(&Request::UpsertVolume {
        id: id.clone(),
        primary: "n1".into(),
    });
    state.apply(&Request::TouchVolume {
        id: id.clone(),
        primary: "n1".into(),
    });
    state.apply(&Request::SetVolumeReplica {
        id: id.clone(),
        node: "n2".into(),
        generation: 2,
    });
    // A late report from an older sync must not undo the newer one.
    state.apply(&Request::SetVolumeReplica {
        id: id.clone(),
        node: "n2".into(),
        generation: 1,
    });
    assert_eq!(state.volumes[&id].replicas["n2"], 2);
}

#[test]
fn an_unused_volume_can_be_removed_and_every_holder_must_ack() {
    let mut state = cluster();
    let id = VolumeId::new("fast", "data");
    state.apply(&Request::UpsertVolume {
        id: id.clone(),
        primary: "n1".into(),
    });
    assert!(state
        .apply(&Request::RemoveVolume { id: id.clone() })
        .is_ok());
    assert!(!state.volumes.contains_key(&id));
    let pending = state.volume_removals[&id].clone();
    assert!(pending.contains("n1"));

    for node in &pending {
        state.apply(&Request::AckVolumeRemoved {
            id: id.clone(),
            node: node.clone(),
        });
    }
    assert!(state.volume_removals.is_empty());
}

#[test]
fn a_mounted_volume_cannot_be_removed() {
    let mut state = cluster();
    let id = VolumeId::new("fast", "data");
    state.apply(&Request::UpsertVolume {
        id: id.clone(),
        primary: "n1".into(),
    });
    state.apply(&Request::SetVolumeEmpty {
        id: id.clone(),
        empty: true,
    });
    state.apply(&Request::DeployStack {
        name: "site".into(),
        spec: "services:\n  web:\n    image: nginx\n    volumes:\n      - fast/data:/data\n".into(),
        at: 0,
    });
    assign(&mut state, "n1", &["fast"]);
    state.apply(&Request::Assign {
        instance: InstanceId::new("site", "web", 1),
        node: "n1".into(),
        drives: drives(&["fast"]),
    });
    let refused = state.apply(&Request::RemoveVolume { id: id.clone() });
    assert!(
        refused.error.is_some_and(|e| e.contains("still mounted")),
        "a mounted volume must not be deleted"
    );
    assert!(state.volumes.contains_key(&id));
}

#[test]
fn a_volume_declared_by_a_stack_cannot_be_removed_even_unassigned() {
    let mut state = cluster();
    let id = VolumeId::new("fast", "data");
    state.apply(&Request::UpsertVolume {
        id: id.clone(),
        primary: "n1".into(),
    });
    state.apply(&Request::DeployStack {
        name: "site".into(),
        spec: "services:\n  web:\n    image: nginx\n    volumes:\n      - fast/data:/data\n".into(),
        at: 0,
    });
    let refused = state.apply(&Request::RemoveVolume { id: id.clone() });
    assert!(refused
        .error
        .is_some_and(|e| e.contains("used by stack site")));
    assert!(state.volumes.contains_key(&id));
}

#[test]
fn volumes_only_live_on_implementing_nodes() {
    let mut state = cluster();
    let rejected = state.apply(&Request::UpsertVolume {
        id: VolumeId::new("bulk", "data"),
        primary: "n2".into(),
    });
    assert!(rejected
        .error
        .unwrap()
        .contains("does not implement drive bulk"));
}
