use super::*;

#[test]
fn clear_errors_drops_failures_and_keeps_everything_else() {
    let mut state = cluster_with_known_drive();
    for id in ["failed", "pending"] {
        assert!(state
            .apply(&Request::PutRestoreRequest(restore_request(id, "fast")))
            .is_ok());
    }
    state.restore_requests.get_mut("failed").unwrap().status =
        RestoreStatus::Failed { error: "x".into() };
    state.cert_status.insert(
        "a.example.com".into(),
        ReconcileStatus::Error {
            message: "x".into(),
            at: 1,
        },
    );
    state
        .dns_status
        .insert("b.example.com".into(), ReconcileStatus::Pending);
    state
        .crash_moves
        .entry(InstanceId::new("gitlab", "web", 0))
        .or_default()
        .insert("n1".into(), 1);

    assert!(state.apply(&Request::ClearErrors).is_ok());

    assert!(state.crash_moves.is_empty());

    assert!(!state.restore_requests.contains_key("failed"));
    assert!(state.restore_requests.contains_key("pending"));
    assert!(state.cert_status.is_empty());
    assert!(state.dns_status.contains_key("b.example.com"));
}

#[test]
fn an_update_keeps_its_start_time_across_phases_and_restarts_it_once_finished() {
    let mut state = cluster();
    assert!(assign(&mut state, "n1", &[]).is_ok());
    let instance = InstanceId::new("gitlab", "web", 0);

    assert!(set_update(&mut state, UpdatePhase::Queued, 10).is_ok());
    assert!(set_update(&mut state, UpdatePhase::Pulling, 20).is_ok());
    assert!(set_update(&mut state, UpdatePhase::Started, 30).is_ok());
    let entry = &state.instance_updates[&instance];
    assert_eq!(
        (entry.phase, entry.started_at, entry.at),
        (UpdatePhase::Started, 10, 30)
    );

    assert!(set_update(&mut state, UpdatePhase::Queued, 40).is_ok());
    assert_eq!(state.instance_updates[&instance].started_at, 40);
}

#[test]
fn auto_update_record_keeps_detection_time_per_digest() {
    let mut state = cluster();
    assert!(assign(&mut state, "n1", &[]).is_ok());
    let instance = InstanceId::new("gitlab", "web", 0);
    let set = |state: &mut ClusterState, digest: &str, outcome, at| {
        state.apply(&Request::SetAutoUpdate {
            instance: InstanceId::new("gitlab", "web", 0),
            digest: digest.into(),
            outcome,
            error: None,
            at,
        })
    };
    assert!(set(&mut state, "sha256:a", AutoUpdateOutcome::Waiting, 10).is_ok());
    assert!(set(&mut state, "sha256:a", AutoUpdateOutcome::Queued, 50).is_ok());
    let rec = &state.auto_updates[&instance];
    assert_eq!((rec.detected_at, rec.at), (10, 50));
    // A newer digest restarts the countdown.
    assert!(set(&mut state, "sha256:b", AutoUpdateOutcome::Waiting, 60).is_ok());
    assert_eq!(state.auto_updates[&instance].detected_at, 60);
    // Survives clearing errors, goes with the instance.
    state.apply(&Request::ClearErrors);
    assert!(state.auto_updates.contains_key(&instance));
    state.apply(&Request::Unassign {
        instance: instance.clone(),
    });
    assert!(state.auto_updates.is_empty());
    assert!(!set(&mut state, "sha256:c", AutoUpdateOutcome::Waiting, 70).is_ok());
}

#[test]
fn image_checks_replace_wholesale_but_keep_a_digest_through_an_error() {
    let mut state = cluster();
    let check = |digest: Option<&str>, error: Option<&str>| ImageCheck {
        latest_digest: digest.map(String::from),
        checked_at: 1,
        error: error.map(String::from),
    };
    state.apply(&Request::SetImageChecks {
        checks: BTreeMap::from([
            ("a:1".to_string(), check(Some("sha256:a"), None)),
            ("b:1".to_string(), check(Some("sha256:b"), None)),
        ]),
        at: 10,
    });
    state.apply(&Request::SetImageChecks {
        checks: BTreeMap::from([("a:1".to_string(), check(None, Some("429")))]),
        at: 20,
    });
    assert_eq!(state.image_checks_at, 20);
    assert_eq!(state.image_checks.len(), 1);
    let a = &state.image_checks["a:1"];
    assert_eq!(a.latest_digest.as_deref(), Some("sha256:a"));
    assert_eq!(a.error.as_deref(), Some("429"));
}

#[test]
fn crash_moves_accumulate_prune_and_clear() {
    let mut state = cluster();
    assert!(assign(&mut state, "n1", &[]).is_ok());
    let instance = InstanceId::new("gitlab", "web", 0);
    let record = |state: &mut ClusterState, from: &str, at: u64| {
        state.apply(&Request::RecordCrashMove {
            instance: instance.clone(),
            from: from.into(),
            at,
        })
    };

    assert!(record(&mut state, "n1", 100).is_ok());
    assert!(record(&mut state, "n2", 200).is_ok());
    assert_eq!(state.crash_moves[&instance].len(), 2);

    // A record far enough later drops the stale entries.
    assert!(record(&mut state, "n3", 200 + CRASH_MOVE_RETENTION_SECS + 1).is_ok());
    assert_eq!(
        state.crash_moves[&instance].keys().collect::<Vec<_>>(),
        vec!["n3"]
    );

    // An instance with no assignment cannot leave an entry behind.
    let rejected = state.apply(&Request::RecordCrashMove {
        instance: InstanceId::new("gitlab", "ghost", 0),
        from: "n1".into(),
        at: 1,
    });
    assert!(rejected.error.is_some());

    // A changed spec forgives every node; an identical redeploy does not.
    state.apply(&Request::DeployStack {
        name: "gitlab".into(),
        spec: "a".into(),
        at: 0,
    });
    assert!(record(&mut state, "n1", 5).is_ok());
    state.apply(&Request::DeployStack {
        name: "gitlab".into(),
        spec: "a".into(),
        at: 0,
    });
    assert!(state.crash_moves.contains_key(&instance));
    state.apply(&Request::DeployStack {
        name: "gitlab".into(),
        spec: "b".into(),
        at: 0,
    });
    assert!(state.crash_moves.is_empty());

    assert!(record(&mut state, "n1", 5).is_ok());
    state.apply(&Request::Unassign {
        instance: instance.clone(),
    });
    assert!(state.crash_moves.is_empty());
}

#[test]
fn update_entries_go_away_with_their_instance() {
    let mut state = cluster();
    assert!(!set_update(&mut state, UpdatePhase::Pulling, 1).is_ok());
    assert!(state.instance_updates.is_empty());

    assert!(assign(&mut state, "n1", &[]).is_ok());
    assert!(set_update(&mut state, UpdatePhase::Pulling, 1).is_ok());
    state.apply(&Request::Unassign {
        instance: InstanceId::new("gitlab", "web", 0),
    });
    assert!(state.instance_updates.is_empty());
}
