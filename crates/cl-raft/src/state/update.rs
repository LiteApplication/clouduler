//! Instance update, image check and error-clearing handlers.

use super::*;

impl ClusterState {
    pub(super) fn apply_update(&mut self, req: &Request) -> Result<(), String> {
        match req {
            Request::SetInstanceUpdate {
                instance,
                node,
                phase,
                error,
                at,
            } => {
                if !self.assignments.contains_key(instance) {
                    return Err(format!("unknown instance {instance}"));
                }
                // A phase change keeps its start time; a fresh update after a finished one restarts it.
                let started_at = match self.instance_updates.get(instance) {
                    Some(current) if !current.phase.is_terminal() => current.started_at,
                    _ => *at,
                };
                self.instance_updates.insert(
                    instance.clone(),
                    InstanceUpdate {
                        node: node.clone(),
                        phase: *phase,
                        started_at,
                        at: *at,
                        error: error.clone(),
                    },
                );
                Ok(())
            }

            Request::ClearInstanceUpdate { instance } => {
                self.instance_updates.remove(instance);
                Ok(())
            }

            Request::SetImageChecks { checks, at } => {
                let mut next = checks.clone();
                for (image, check) in next.iter_mut() {
                    if check.latest_digest.is_none() && check.error.is_some() {
                        check.latest_digest = self
                            .image_checks
                            .get(image)
                            .and_then(|old| old.latest_digest.clone());
                    }
                }
                self.image_checks = next;
                self.image_checks_at = *at;
                Ok(())
            }

            Request::SetAutoUpdate {
                instance,
                digest,
                outcome,
                error,
                at,
            } => {
                if !self.assignments.contains_key(instance) {
                    return Err(format!("unknown instance {instance}"));
                }
                let detected_at = match self.auto_updates.get(instance) {
                    Some(current) if &current.digest == digest => current.detected_at,
                    _ => *at,
                };
                self.auto_updates.insert(
                    instance.clone(),
                    AutoUpdateRecord {
                        digest: digest.clone(),
                        detected_at,
                        outcome: *outcome,
                        at: *at,
                        error: error.clone(),
                    },
                );
                Ok(())
            }

            Request::ClearErrors => {
                self.restic_runs
                    .retain(|_, r| !matches!(r.outcome, BackupRunOutcome::Failed { .. }));
                self.restore_requests
                    .retain(|_, r| !matches!(r.status, RestoreStatus::Failed { .. }));
                self.instance_updates
                    .retain(|_, u| !matches!(u.phase, UpdatePhase::Failed));
                for assignment in self.assignments.values_mut() {
                    if matches!(
                        assignment.migration,
                        Some(Migration {
                            phase: MigrationPhase::Failed { .. },
                            ..
                        })
                    ) {
                        assignment.migration = None;
                    }
                }
                // A still-failing reconciler rewrites its error next pass, so this only clears stale ones.
                for statuses in [&mut self.cert_status, &mut self.dns_status] {
                    statuses.retain(|_, s| !matches!(s, ReconcileStatus::Error { .. }));
                }
                self.alert_destinations.retain(|_, h| h.ok);
                Ok(())
            }

            _ => unreachable!("apply_update was handed a request routed elsewhere"),
        }
    }
}
