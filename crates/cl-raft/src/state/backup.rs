//! Backup destination, target, run and restore handlers.

use super::*;

impl ClusterState {
    pub(super) fn apply_backup(&mut self, req: &Request) -> Result<(), String> {
        match req {
            Request::PutBackupDestination(destination) => {
                validate_object_name(&destination.id)?;
                crate::backup_validate::validate_destination(destination)?;
                if let BackupDestinationKind::NodeLocal { node, .. } = &destination.kind {
                    if !self.nodes.contains_key(node) {
                        return Err(format!("unknown node {node}"));
                    }
                }
                self.restic_destinations
                    .insert(destination.id.clone(), destination.clone());
                Ok(())
            }

            Request::RemoveBackupDestination { id } => {
                if let Some(user) = self.destination_in_use(id) {
                    return Err(format!("backup destination {id} is still used by {user}"));
                }
                self.restic_destinations.remove(id);
                Ok(())
            }

            Request::PutBackupTarget(target) => {
                validate_object_name(&target.id)?;
                if target.frequency_secs == 0 {
                    // Would be due on every scheduler poll, starting a new run the instant the last finished.
                    return Err("frequency_secs must be at least 1".to_string());
                }
                if !self.drive_known(&target.drive) {
                    return Err(format!("unknown drive {}", target.drive));
                }
                if !self
                    .restic_destinations
                    .contains_key(&target.destination_id)
                {
                    return Err(format!(
                        "unknown backup destination {}",
                        target.destination_id
                    ));
                }
                if target.enabled
                    && self.restic_targets.values().any(|t| {
                        t.id != target.id
                            && t.enabled
                            && t.drive == target.drive
                            && t.destination_id == target.destination_id
                    })
                {
                    return Err(format!(
                        "drive {} already has an enabled backup target for destination {}",
                        target.drive, target.destination_id
                    ));
                }
                self.restic_targets
                    .insert(target.id.clone(), target.clone());
                Ok(())
            }

            Request::RemoveBackupTarget { id } => {
                self.restic_targets.remove(id);
                Ok(())
            }

            Request::SetBackupTargetLastRun { id, at } => {
                let target = self
                    .restic_targets
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown backup target {id}"))?;
                target.last_run = Some(*at);
                Ok(())
            }

            Request::RecordBackupRun(run) => {
                // Progress/finish for a run's own id is always allowed; only a new run competing for the target is rejected.
                if matches!(run.outcome, BackupRunOutcome::Running { .. })
                    && self.restic_runs.values().any(|r| {
                        r.target_id == run.target_id
                            && r.id != run.id
                            && matches!(r.outcome, BackupRunOutcome::Running { .. })
                    })
                {
                    return Err(format!(
                        "a backup is already running for target {}",
                        run.target_id
                    ));
                }
                self.restic_runs.insert(run.id.clone(), run.clone());
                let mut same_target: Vec<&BackupRun> = self
                    .restic_runs
                    .values()
                    .filter(|r| r.drive == run.drive && r.destination_id == run.destination_id)
                    .collect();
                if same_target.len() > MAX_BACKUP_RUNS_PER_TARGET {
                    // Oldest first, never the run just recorded (may carry an old timestamp); finished before still Running.
                    same_target.retain(|r| r.id != run.id);
                    same_target.sort_by_key(|r| {
                        (
                            matches!(r.outcome, BackupRunOutcome::Running { .. }),
                            r.started_at,
                        )
                    });
                    let drop_count =
                        (same_target.len() + 1 - MAX_BACKUP_RUNS_PER_TARGET).min(same_target.len());
                    let to_drop: Vec<String> = same_target
                        .iter()
                        .take(drop_count)
                        .map(|r| r.id.clone())
                        .collect();
                    for id in to_drop {
                        self.restic_runs.remove(&id);
                    }
                }
                Ok(())
            }

            Request::PutRestoreRequest(request) => {
                validate_object_name(&request.id)?;
                match &request.scope {
                    RestoreScope::Drive { drive }
                    | RestoreScope::Volume {
                        volume: VolumeId { drive, .. },
                    } => {
                        if !self.drive_known(drive) {
                            return Err(format!("unknown drive {drive}"));
                        }
                    }
                    RestoreScope::Service { stack, .. } => {
                        if !self.stacks.contains_key(stack) {
                            return Err(format!("unknown stack {stack}"));
                        }
                    }
                }
                if let Some(destination_id) = &request.destination_id {
                    if !self.restic_destinations.contains_key(destination_id) {
                        return Err(format!("unknown backup destination {destination_id}"));
                    }
                }
                let mut request = request.clone();
                request.status = RestoreStatus::Pending;
                request.finished_at = None;
                let request_id = request.id.clone();
                self.restore_requests.insert(request_id.clone(), request);

                if self.restore_requests.len() > MAX_RESTORE_REQUESTS {
                    // Never the request just inserted (old timestamp possible); finished before Pending/Running.
                    let mut all: Vec<&RestoreRequest> = self
                        .restore_requests
                        .values()
                        .filter(|r| r.id != request_id)
                        .collect();
                    all.sort_by_key(|r| {
                        (
                            matches!(r.status, RestoreStatus::Pending | RestoreStatus::Running),
                            r.requested_at,
                        )
                    });
                    let drop_count = (all.len() + 1 - MAX_RESTORE_REQUESTS).min(all.len());
                    let to_drop: Vec<String> =
                        all.iter().take(drop_count).map(|r| r.id.clone()).collect();
                    for id in to_drop {
                        self.restore_requests.remove(&id);
                    }
                }
                Ok(())
            }

            Request::SetRestoreStatus { id, status, at } => {
                let request = self
                    .restore_requests
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown restore request {id}"))?;
                if matches!(
                    request.status,
                    RestoreStatus::Succeeded | RestoreStatus::Failed { .. }
                ) {
                    return Err(format!("restore request {id} already finished"));
                }
                if matches!(status, RestoreStatus::Running)
                    && !matches!(request.status, RestoreStatus::Pending)
                {
                    return Err(format!(
                        "restore request {id} is not pending, cannot be claimed"
                    ));
                }
                if matches!(
                    status,
                    RestoreStatus::Succeeded | RestoreStatus::Failed { .. }
                ) {
                    request.finished_at = Some(*at);
                }
                request.status = status.clone();
                Ok(())
            }

            _ => unreachable!("apply_backup was handed a request routed elsewhere"),
        }
    }
}
