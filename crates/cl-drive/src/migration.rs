//! The migration handoff: `stop -> final sync -> verify -> switch -> start -> done`.
//!
//! Every step is recorded through raft before or after doing its work, so
//! `run` can be called again after a crash and it picks up from whatever
//! phase is on record instead of repeating (or skipping) work. This module
//! never touches docker: starting and stopping the container is delegated to
//! `InstanceControl`, which the daemon implements over `cl-docker`. That
//! keeps migration logic testable with a fake and no docker socket.

use std::net::SocketAddr;

use cl_raft::state::MigrationPhase;
use cl_raft::{ClusterNode, InstanceId, Request, VolumeId};

use crate::delta::{BoxFuture, SyncEngine};
use crate::error::{DriveError, Result};
use crate::registry::DriveRegistry;
use crate::scan;
use crate::transport;

/// Starts and stops the container being moved. Implemented by the scheduler
/// crate over `cl-docker`; a fake implementation is all migration.rs needs
/// to be exercised in tests.
pub trait InstanceControl: Send + Sync {
    fn stop_instance<'a>(&'a self, instance: &'a InstanceId, node: &'a str) -> BoxFuture<'a, Result<()>>;
    fn start_instance<'a>(&'a self, instance: &'a InstanceId, node: &'a str) -> BoxFuture<'a, Result<()>>;
}

/// Everything needed to carry one migration through to completion.
pub struct MigrationJob<'a> {
    pub instance: InstanceId,
    pub source_node: String,
    pub target_node: String,
    /// Address of the target's drive transport listener.
    pub target_addr: SocketAddr,
    /// Volumes the instance mounts, all moving together.
    pub volumes: Vec<VolumeId>,
    pub registry: &'a DriveRegistry,
    pub engine: &'a dyn SyncEngine,
    pub control: &'a dyn InstanceControl,
}

/// Drive `instance`'s migration through to `Done` or `Failed`, resuming from
/// whatever phase raft has recorded. Returns once the move is finished or
/// has failed; a failure leaves the source assignment and volume primary
/// untouched, since nothing here calls `Switching` until every earlier step
/// has succeeded.
pub async fn run(node: &ClusterNode, job: &MigrationJob<'_>) -> Result<()> {
    // Read once, at the door. `node.state()` is this node's applied state, and
    // on anyone but the leader it lags the write that has just been
    // acknowledged: re-reading it per iteration hands back the phase that was
    // just left, and the step runs a second time. That is merely noisy for a
    // stop and actively wrong for `Switching`, which bumps every volume's
    // generation. So raft records each phase, and the loop then trusts the
    // phase it just recorded; the read is for resuming, not for stepping.
    let mut phase = recorded_phase(node, &job.instance).await?;

    loop {
        match phase {
            MigrationPhase::Stopping => {
                if let Err(e) = job.control.stop_instance(&job.instance, &job.source_node).await {
                    return Err(fail(node, &job.instance, e).await);
                }
                phase = advance(node, &job.instance, MigrationPhase::Syncing).await?;
            }

            MigrationPhase::Syncing => {
                if let Err(e) = sync_all(node, job).await {
                    return Err(fail(node, &job.instance, e).await);
                }
                phase = advance(node, &job.instance, MigrationPhase::Verifying).await?;
            }

            MigrationPhase::Verifying => {
                if let Err(e) = verify_all(job).await {
                    return Err(fail(node, &job.instance, e).await);
                }
                phase = advance(node, &job.instance, MigrationPhase::Switching).await?;
            }

            MigrationPhase::Switching => {
                // The point of no return: every volume's primary moves to the
                // target. Only reached once sync and verify both succeeded.
                for volume in &job.volumes {
                    node.submit(Request::TouchVolume { id: volume.clone(), primary: job.target_node.clone() })
                        .await
                        .map_err(|e| DriveError::Transfer { message: e.to_string() })?;
                }
                phase = advance(node, &job.instance, MigrationPhase::Starting).await?;
            }

            MigrationPhase::Starting => {
                if let Err(e) = job.control.start_instance(&job.instance, &job.target_node).await {
                    return Err(fail(node, &job.instance, e).await);
                }
                advance(node, &job.instance, MigrationPhase::Done).await?;
                // `Done` clears the migration record (state.rs), so there is
                // nothing left to read back; the move is over.
                return Ok(());
            }

            MigrationPhase::Done => return Ok(()),

            MigrationPhase::Failed { reason } => return Err(DriveError::Transfer { message: reason }),
        }
    }
}

/// The phase raft has on record for `instance`, which is where a resumed
/// migration picks up.
async fn recorded_phase(node: &ClusterNode, instance: &InstanceId) -> Result<MigrationPhase> {
    let state = node.state().await;
    let assignment = state
        .assignments
        .get(instance)
        .ok_or_else(|| DriveError::Config(format!("{instance} is not assigned")))?;
    assignment
        .migration
        .as_ref()
        .map(|migration| migration.phase.clone())
        .ok_or_else(|| DriveError::Config(format!("{instance} is not migrating")))
}

async fn sync_all(node: &ClusterNode, job: &MigrationJob<'_>) -> Result<()> {
    for volume in &job.volumes {
        let state = node.state().await;
        let generation = state
            .volumes
            .get(volume)
            .ok_or_else(|| DriveError::VolumeNotFound { volume: volume.clone() })?
            .generation;

        let src_path = job.registry.volume_path(volume)?;
        job.engine.sync(&src_path, job.target_addr, volume, generation).await?;

        node.submit(Request::SetVolumeReplica {
            id: volume.clone(),
            node: job.target_node.clone(),
            generation,
        })
        .await
        .map_err(|e| DriveError::Transfer { message: e.to_string() })?;
    }
    Ok(())
}

async fn verify_all(job: &MigrationJob<'_>) -> Result<()> {
    for volume in &job.volumes {
        let src_path = job.registry.volume_path(volume)?;
        let source = scan::scan(&src_path, true)?;
        let target = transport::request_manifest(job.target_addr, volume, true).await?;

        let diff = source.diff(&target);
        if !diff.added.is_empty() || !diff.changed.is_empty() || !diff.removed.is_empty() {
            return Err(DriveError::Verify {
                message: format!(
                    "{volume}: target differs after sync (added={:?} changed={:?} removed={:?})",
                    diff.added, diff.changed, diff.removed
                ),
            });
        }
    }
    Ok(())
}

/// Record `phase` and hand it back, so the caller carries on from what it
/// wrote rather than from a state read that may not have caught up yet.
async fn advance(
    node: &ClusterNode,
    instance: &InstanceId,
    phase: MigrationPhase,
) -> Result<MigrationPhase> {
    let response = node
        .submit(Request::AdvanceMigration {
            instance: instance.clone(),
            phase: phase.clone(),
        })
        .await
        .map_err(|e| DriveError::Transfer { message: e.to_string() })?;
    if let Some(err) = response.error {
        return Err(DriveError::Transfer { message: err });
    }
    Ok(phase)
}

/// Record the failure through raft (best effort: if this write also fails
/// there is nothing more we can do) and hand back the original error.
async fn fail(node: &ClusterNode, instance: &InstanceId, err: DriveError) -> DriveError {
    let _ = node
        .submit(Request::AdvanceMigration {
            instance: instance.clone(),
            phase: MigrationPhase::Failed { reason: err.to_string() },
        })
        .await;
    err
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysOk;
    impl InstanceControl for AlwaysOk {
        fn stop_instance<'a>(&'a self, _i: &'a InstanceId, _n: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn start_instance<'a>(&'a self, _i: &'a InstanceId, _n: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    // Full phase-ordering behaviour needs a running raft node and a real
    // transport listener, so it lives in tests/migration.rs as an
    // integration test. This just checks the trait is object safe and easy
    // to fake, which is the property the design is meant to guarantee.
    #[test]
    fn instance_control_is_object_safe() {
        let _: Box<dyn InstanceControl> = Box::new(AlwaysOk);
    }
}
