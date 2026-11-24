use super::*;

#[test]
fn a_node_local_destination_must_name_a_known_node() {
    let mut state = cluster();
    let rejected = state.apply(&Request::PutBackupDestination(node_local_destination(
        "d1", "nowhere",
    )));
    assert!(rejected.error.unwrap().contains("unknown node nowhere"));
}

#[test]
fn a_backup_target_must_name_a_known_drive_and_destination() {
    let mut state = cluster_with_known_drive();
    state.apply(&Request::PutBackupDestination(node_local_destination(
        "d1", "n1",
    )));

    let unknown_drive = state.apply(&Request::PutBackupTarget(backup_target(
        "t1", "nowhere", "d1", true,
    )));
    assert!(unknown_drive
        .error
        .unwrap()
        .contains("unknown drive nowhere"));

    let unknown_destination = state.apply(&Request::PutBackupTarget(backup_target(
        "t1", "fast", "nowhere", true,
    )));
    assert!(unknown_destination
        .error
        .unwrap()
        .contains("unknown backup destination nowhere"));

    assert!(state
        .apply(&Request::PutBackupTarget(backup_target(
            "t1", "fast", "d1", true
        )))
        .is_ok());
    assert!(state.restic_targets.contains_key("t1"));
}

#[test]
fn a_zero_frequency_target_is_rejected() {
    let mut state = cluster_with_known_drive();
    state.apply(&Request::PutBackupDestination(node_local_destination(
        "d1", "n1",
    )));
    let mut target = backup_target("t1", "fast", "d1", true);
    target.frequency_secs = 0;
    let rejected = state.apply(&Request::PutBackupTarget(target));
    assert!(rejected.error.unwrap().contains("frequency_secs"));
    assert!(!state.restic_targets.contains_key("t1"));
}

#[test]
fn an_ssh_destination_with_an_option_injecting_host_is_rejected() {
    let mut state = cluster();
    let rejected = state.apply(&Request::PutBackupDestination(BackupDestination {
        id: "d1".into(),
        name: "d1".into(),
        kind: BackupDestinationKind::Ssh {
            host: "-oProxyCommand=touch".into(),
            port: 22,
            user: "u".into(),
            path: "/p".into(),
        },
        password_secret: "_clouduler_restic_password_d1".into(),
        credentials_secret: None,
    }));
    assert!(rejected.error.unwrap().contains("ssh host"));
    assert!(state.restic_destinations.is_empty());
}

#[test]
fn two_enabled_targets_cannot_share_a_drive_and_destination() {
    let mut state = cluster_with_known_drive();
    state.apply(&Request::PutBackupDestination(node_local_destination(
        "d1", "n1",
    )));
    state.apply(&Request::PutBackupTarget(backup_target(
        "t1", "fast", "d1", true,
    )));

    let rejected = state.apply(&Request::PutBackupTarget(backup_target(
        "t2", "fast", "d1", true,
    )));
    assert!(rejected
        .error
        .unwrap()
        .contains("already has an enabled backup target"));

    // A disabled duplicate is fine (e.g. a draft being edited).
    assert!(state
        .apply(&Request::PutBackupTarget(backup_target(
            "t2", "fast", "d1", false
        )))
        .is_ok());
}

#[test]
fn removing_a_destination_in_use_by_a_target_is_refused() {
    let mut state = cluster_with_known_drive();
    state.apply(&Request::PutBackupDestination(node_local_destination(
        "d1", "n1",
    )));
    state.apply(&Request::PutBackupTarget(backup_target(
        "t1", "fast", "d1", true,
    )));

    let rejected = state.apply(&Request::RemoveBackupDestination { id: "d1".into() });
    assert!(rejected
        .error
        .unwrap()
        .contains("still used by backup target t1"));

    state.apply(&Request::RemoveBackupTarget { id: "t1".into() });
    assert!(state
        .apply(&Request::RemoveBackupDestination { id: "d1".into() })
        .is_ok());
    assert!(!state.restic_destinations.contains_key("d1"));
}

#[test]
fn setting_last_run_updates_only_the_named_target() {
    let mut state = cluster_with_known_drive();
    state.apply(&Request::PutBackupDestination(node_local_destination(
        "d1", "n1",
    )));
    state.apply(&Request::PutBackupTarget(backup_target(
        "t1", "fast", "d1", true,
    )));

    assert!(state
        .apply(&Request::SetBackupTargetLastRun {
            id: "t1".into(),
            at: 100
        })
        .is_ok());
    assert_eq!(state.restic_targets["t1"].last_run, Some(100));

    let rejected = state.apply(&Request::SetBackupTargetLastRun {
        id: "nowhere".into(),
        at: 0,
    });
    assert!(rejected
        .error
        .unwrap()
        .contains("unknown backup target nowhere"));
}

#[test]
fn a_second_running_run_for_the_same_target_is_rejected() {
    let mut state = cluster_with_known_drive();
    let first = BackupRun {
        id: "run1".into(),
        target_id: "t1".into(),
        drive: "fast".into(),
        destination_id: "d1".into(),
        started_at: 0,
        finished_at: None,
        outcome: BackupRunOutcome::Running { percent: None },
        triggered_by: BackupTrigger::Schedule,
    };
    assert!(state
        .apply(&Request::RecordBackupRun(first.clone()))
        .is_ok());

    let second = BackupRun {
        id: "run2".into(),
        ..first.clone()
    };
    let rejected = state.apply(&Request::RecordBackupRun(second));
    assert!(rejected.error.unwrap().contains("already running"));

    // A progress update against the same run id is not a new run and is
    // always allowed.
    let mut progressed = first.clone();
    progressed.outcome = BackupRunOutcome::Running { percent: Some(0.5) };
    assert!(state.apply(&Request::RecordBackupRun(progressed)).is_ok());

    // Finishing the first run frees the target up for a new one.
    let mut finished = first;
    finished.outcome = BackupRunOutcome::Succeeded {
        snapshot_id: "abc".into(),
        bytes: 0,
        warning: None,
    };
    assert!(state.apply(&Request::RecordBackupRun(finished)).is_ok());
    let third = BackupRun {
        id: "run3".into(),
        target_id: "t1".into(),
        drive: "fast".into(),
        destination_id: "d1".into(),
        started_at: 1,
        finished_at: None,
        outcome: BackupRunOutcome::Running { percent: None },
        triggered_by: BackupTrigger::Schedule,
    };
    assert!(state.apply(&Request::RecordBackupRun(third)).is_ok());
}

#[test]
fn recording_a_run_prunes_older_runs_past_the_cap_for_that_target() {
    let mut state = cluster_with_known_drive();
    for i in 0..(MAX_BACKUP_RUNS_PER_TARGET + 5) {
        state.apply(&Request::RecordBackupRun(BackupRun {
            id: format!("run{i}"),
            target_id: "t1".into(),
            drive: "fast".into(),
            destination_id: "d1".into(),
            started_at: i as i64,
            finished_at: Some(i as i64 + 1),
            outcome: BackupRunOutcome::Succeeded {
                snapshot_id: "abc".into(),
                bytes: 0,
                warning: None,
            },
            triggered_by: BackupTrigger::Schedule,
        }));
    }
    let for_target: Vec<_> = state
        .restic_runs
        .values()
        .filter(|r| r.drive == "fast" && r.destination_id == "d1")
        .collect();
    assert_eq!(for_target.len(), MAX_BACKUP_RUNS_PER_TARGET);
    // The oldest runs (run0..run4) were the ones dropped.
    assert!(!state.restic_runs.contains_key("run0"));
    assert!(state
        .restic_runs
        .contains_key(&format!("run{}", MAX_BACKUP_RUNS_PER_TARGET + 4)));
}

#[test]
fn a_restore_request_must_name_a_known_drive() {
    let mut state = cluster();
    let rejected = state.apply(&Request::PutRestoreRequest(restore_request(
        "r1", "nowhere",
    )));
    assert!(rejected.error.unwrap().contains("unknown drive nowhere"));
}

#[test]
fn a_restore_request_always_lands_pending_regardless_of_the_caller() {
    let mut state = cluster_with_known_drive();
    let mut request = restore_request("r1", "fast");
    request.status = RestoreStatus::Succeeded;
    request.finished_at = Some(999);
    assert!(state.apply(&Request::PutRestoreRequest(request)).is_ok());
    assert_eq!(state.restore_requests["r1"].status, RestoreStatus::Pending);
    assert_eq!(state.restore_requests["r1"].finished_at, None);
}

#[test]
fn only_the_first_claim_of_a_pending_restore_succeeds() {
    let mut state = cluster_with_known_drive();
    state.apply(&Request::PutRestoreRequest(restore_request("r1", "fast")));

    assert!(state
        .apply(&Request::SetRestoreStatus {
            id: "r1".into(),
            status: RestoreStatus::Running,
            at: 10
        })
        .is_ok());
    let rejected = state.apply(&Request::SetRestoreStatus {
        id: "r1".into(),
        status: RestoreStatus::Running,
        at: 11,
    });
    assert!(rejected.error.unwrap().contains("not pending"));
}

#[test]
fn a_terminal_restore_status_cannot_be_advanced_further() {
    let mut state = cluster_with_known_drive();
    state.apply(&Request::PutRestoreRequest(restore_request("r1", "fast")));
    state.apply(&Request::SetRestoreStatus {
        id: "r1".into(),
        status: RestoreStatus::Running,
        at: 10,
    });
    assert!(state
        .apply(&Request::SetRestoreStatus {
            id: "r1".into(),
            status: RestoreStatus::Failed {
                error: "boom".into()
            },
            at: 20,
        })
        .is_ok());
    assert_eq!(state.restore_requests["r1"].finished_at, Some(20));

    let rejected = state.apply(&Request::SetRestoreStatus {
        id: "r1".into(),
        status: RestoreStatus::Running,
        at: 30,
    });
    assert!(rejected.error.unwrap().contains("already finished"));
}

#[test]
fn run_pruning_never_drops_the_new_entry_or_a_running_one() {
    let mut state = cluster_with_known_drive();
    // An old Running run, then a full set of finished runs newer than it.
    assert!(state
        .apply(&Request::RecordBackupRun(running_run("old-running", 0)))
        .is_ok());
    for i in 0..MAX_BACKUP_RUNS_PER_TARGET {
        let mut r = running_run(&format!("done{i}"), 100 + i as i64);
        r.outcome = BackupRunOutcome::Succeeded {
            snapshot_id: "s".into(),
            bytes: 0,
            warning: None,
        };
        assert!(state.apply(&Request::RecordBackupRun(r)).is_ok());
    }
    // A new entry with the oldest timestamp of all.
    let mut ancient = running_run("ancient", -5);
    ancient.outcome = BackupRunOutcome::Succeeded {
        snapshot_id: "s".into(),
        bytes: 0,
        warning: None,
    };
    assert!(state.apply(&Request::RecordBackupRun(ancient)).is_ok());
    assert!(state.restic_runs.contains_key("ancient"));
    assert!(state.restic_runs.contains_key("old-running"));
    assert_eq!(state.restic_runs.len(), MAX_BACKUP_RUNS_PER_TARGET);
    assert!(!state.restic_runs.contains_key("done0"));
}

#[test]
fn restore_pruning_never_drops_the_new_entry() {
    let mut state = cluster_with_known_drive();
    for i in 0..MAX_RESTORE_REQUESTS {
        let mut r = restore_request(&format!("r{i}"), "fast");
        r.requested_at = 100 + i as i64;
        assert!(state.apply(&Request::PutRestoreRequest(r)).is_ok());
    }
    let mut ancient = restore_request("ancient", "fast");
    ancient.requested_at = -1;
    assert!(state.apply(&Request::PutRestoreRequest(ancient)).is_ok());
    assert!(state.restore_requests.contains_key("ancient"));
    assert_eq!(state.restore_requests.len(), MAX_RESTORE_REQUESTS);
}
