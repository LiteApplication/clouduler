//! Instance migration handlers.

use super::*;

impl ClusterState {
    pub(super) fn apply_migration(&mut self, req: &Request) -> Result<(), String> {
        match req {
            Request::BeginMigration {
                instance,
                to,
                drives,
            } => {
                self.check_placement(to, drives)?;
                let assignment = self.assignment_mut(instance)?;
                if assignment.node == *to {
                    return Err(format!("{instance} already runs on {to}"));
                }
                if let Some(active) = &assignment.migration {
                    if !active.phase.is_terminal() {
                        return Err(format!("{instance} is already migrating to {}", active.to));
                    }
                }
                assignment.migration = Some(Migration {
                    from: assignment.node.clone(),
                    to: to.clone(),
                    phase: MigrationPhase::Stopping,
                });
                Ok(())
            }

            Request::AdvanceMigration { instance, phase } => {
                let assignment = self.assignment_mut(instance)?;
                let migration = assignment
                    .migration
                    .as_mut()
                    .ok_or_else(|| format!("{instance} is not migrating"))?;

                // The phase already on record succeeds, so a driver retrying a submit it never
                // saw answered can tell "already done" from "not allowed".
                if &migration.phase == phase {
                    return Ok(());
                }

                // Failure can interrupt anywhere; forward progress must follow phase order so a stale message cannot rewind.
                let allowed = matches!(phase, MigrationPhase::Failed { .. })
                    || migration.phase.next().as_ref() == Some(phase);
                if !allowed {
                    return Err(format!(
                        "cannot advance {instance} from {:?} to {phase:?}",
                        migration.phase
                    ));
                }

                migration.phase = phase.clone();
                match phase {
                    // Switching is the point of no return: the target owns the instance.
                    MigrationPhase::Switching => assignment.node = migration.to.clone(),
                    MigrationPhase::Done => assignment.migration = None,
                    _ => {}
                }
                Ok(())
            }

            Request::ClearMigration { instance } => {
                let assignment = self.assignment_mut(instance)?;
                match &assignment.migration {
                    Some(Migration {
                        phase: MigrationPhase::Failed { .. },
                        ..
                    }) => {
                        assignment.migration = None;
                        Ok(())
                    }
                    Some(_) => Err(format!("{instance} is still migrating")),
                    None => Err(format!("{instance} has no failed move to clear")),
                }
            }

            _ => unreachable!("apply_migration was handed a request routed elsewhere"),
        }
    }
}
