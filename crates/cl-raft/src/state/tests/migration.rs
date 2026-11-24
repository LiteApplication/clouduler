use super::*;

#[test]
fn migration_walks_the_phases_in_order() {
    let mut state = cluster();
    let instance = InstanceId::new("gitlab", "web", 0);
    assign(&mut state, "n1", &["fast"]);

    assert!(state
        .apply(&Request::BeginMigration {
            instance: instance.clone(),
            to: "n2".into(),
            drives: drives(&["fast"]),
        })
        .is_ok());

    // Skipping ahead is refused, so a stale message cannot rewind a move.
    let skipped = state.apply(&Request::AdvanceMigration {
        instance: instance.clone(),
        phase: MigrationPhase::Starting,
    });
    assert!(skipped.error.unwrap().contains("cannot advance"));

    for phase in [
        MigrationPhase::Syncing,
        MigrationPhase::Verifying,
        MigrationPhase::Switching,
    ] {
        assert!(state
            .apply(&Request::AdvanceMigration {
                instance: instance.clone(),
                phase,
            })
            .is_ok());
    }
    // Ownership changes at the switch, before the container starts.
    assert_eq!(state.assignments[&instance].node, "n2");

    state.apply(&Request::AdvanceMigration {
        instance: instance.clone(),
        phase: MigrationPhase::Starting,
    });
    state.apply(&Request::AdvanceMigration {
        instance: instance.clone(),
        phase: MigrationPhase::Done,
    });
    assert!(state.assignments[&instance].migration.is_none());
}

#[test]
fn clear_migration_only_clears_a_failed_record() {
    let mut state = cluster();
    let instance = InstanceId::new("gitlab", "web", 0);
    assign(&mut state, "n1", &["fast"]);
    let clear = Request::ClearMigration {
        instance: instance.clone(),
    };

    // Nothing to clear.
    assert!(state
        .apply(&clear)
        .error
        .unwrap()
        .contains("no failed move"));

    state.apply(&Request::BeginMigration {
        instance: instance.clone(),
        to: "n2".into(),
        drives: drives(&["fast"]),
    });
    // Active move is refused.
    assert!(state
        .apply(&clear)
        .error
        .unwrap()
        .contains("still migrating"));
    assert!(state.assignments[&instance].migration.is_some());

    state.apply(&Request::AdvanceMigration {
        instance: instance.clone(),
        phase: MigrationPhase::Failed {
            reason: "boom".into(),
        },
    });
    assert!(state.apply(&clear).is_ok());
    assert!(state.assignments[&instance].migration.is_none());
    assert_eq!(state.assignments[&instance].node, "n1");
}

#[test]
fn re_recording_the_current_phase_is_a_no_op() {
    let mut state = cluster();
    let instance = InstanceId::new("gitlab", "web", 0);
    assign(&mut state, "n1", &["fast"]);

    state.apply(&Request::BeginMigration {
        instance: instance.clone(),
        to: "n2".into(),
        drives: drives(&["fast"]),
    });
    state.apply(&Request::AdvanceMigration {
        instance: instance.clone(),
        phase: MigrationPhase::Syncing,
    });

    // A driver that retried a submit it never saw the answer to must be
    // told "already done", not "not allowed": the second reads as a
    // failure and would abandon a move that is in fact on track.
    let repeat = state.apply(&Request::AdvanceMigration {
        instance: instance.clone(),
        phase: MigrationPhase::Syncing,
    });
    assert!(repeat.error.is_none());
    assert_eq!(
        state.assignments[&instance]
            .migration
            .as_ref()
            .unwrap()
            .phase,
        MigrationPhase::Syncing
    );

    // Going backwards is still refused.
    let rewind = state.apply(&Request::AdvanceMigration {
        instance: instance.clone(),
        phase: MigrationPhase::Stopping,
    });
    assert!(rewind.error.unwrap().contains("cannot advance"));
}

#[test]
fn a_migration_records_the_node_driving_it() {
    let mut state = cluster();
    let instance = InstanceId::new("gitlab", "web", 0);
    assign(&mut state, "n1", &["fast"]);

    state.apply(&Request::BeginMigration {
        instance: instance.clone(),
        to: "n2".into(),
        drives: drives(&["fast"]),
    });
    let migration = state.assignments[&instance].migration.clone().unwrap();
    assert_eq!(migration.from, "n1");

    // `Switching` repoints the assignment, so `from` is the only thing
    // that still says whose job this is.
    for phase in [
        MigrationPhase::Syncing,
        MigrationPhase::Verifying,
        MigrationPhase::Switching,
    ] {
        state.apply(&Request::AdvanceMigration {
            instance: instance.clone(),
            phase,
        });
    }
    assert_eq!(state.assignments[&instance].node, "n2");
    assert_eq!(
        state.assignments[&instance]
            .migration
            .as_ref()
            .unwrap()
            .from,
        "n1"
    );
}

#[test]
fn migration_target_must_implement_the_drives() {
    let mut state = cluster();
    let instance = InstanceId::new("gitlab", "web", 0);
    assign(&mut state, "n1", &["bulk"]);
    let rejected = state.apply(&Request::BeginMigration {
        instance,
        to: "n2".into(),
        drives: drives(&["bulk"]),
    });
    assert!(rejected
        .error
        .unwrap()
        .contains("does not implement drive bulk"));
}

#[test]
fn migration_can_fail_from_any_phase() {
    let mut state = cluster();
    let instance = InstanceId::new("gitlab", "web", 0);
    assign(&mut state, "n1", &["fast"]);
    state.apply(&Request::BeginMigration {
        instance: instance.clone(),
        to: "n2".into(),
        drives: drives(&["fast"]),
    });
    assert!(state
        .apply(&Request::AdvanceMigration {
            instance: instance.clone(),
            phase: MigrationPhase::Failed {
                reason: "sync timed out".into()
            },
        })
        .is_ok());
    // The instance stays on the source node when the move fails early.
    assert_eq!(state.assignments[&instance].node, "n1");
}

#[test]
fn one_migration_at_a_time() {
    let mut state = cluster();
    let instance = InstanceId::new("gitlab", "web", 0);
    assign(&mut state, "n1", &["fast"]);
    state.apply(&Request::BeginMigration {
        instance: instance.clone(),
        to: "n2".into(),
        drives: drives(&["fast"]),
    });
    let rejected = state.apply(&Request::BeginMigration {
        instance,
        to: "n3".into(),
        drives: drives(&[]),
    });
    assert!(rejected.error.unwrap().contains("already migrating"));
}
