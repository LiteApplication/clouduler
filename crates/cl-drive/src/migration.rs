//! The migration handoff: `stop -> final sync -> verify -> switch -> start -> done`.
//!
//! Every step is recorded through raft before or after doing its work, so
//! `run` can be called again after a crash and it picks up from whatever
//! phase is on record instead of repeating (or skipping) work. This module
//! never touches docker: starting and stopping the container is delegated to
//! `InstanceControl`, which the daemon implements over `cl-docker`. That
//! keeps migration logic testable with a fake and no docker socket.
//!
//! A caller driving several migrations off the same node at once is expected
//! to share one `MigrationJob::transfer_gate` between them: `run` holds it
//! for exactly as long as the instance is down and its volumes are in
//! flight (`Stopping`/`Syncing`), and releases it the moment the data has
//! landed. `Verifying` onward — hashing the copy, switching the volume's
//! primary, starting the target, tidying up the source — all run with the
//! gate released, so the next queued migration can already be stopping its
//! own container and moving its own volume by then. Only one instance is
//! ever down for its own move at a time; nothing waits on another's tail.

use std::net::SocketAddr;
use std::sync::Arc;

use cl_raft::state::MigrationPhase;
use cl_raft::{ClusterNode, InstanceId, Request, VolumeId};
use tokio::sync::Semaphore;
use tracing::warn;

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
    /// Take the stopped container away on the node it was moved off.
    ///
    /// Must treat an already-absent container as success: the source node's
    /// own reconcile loop removes containers it is no longer assigned, so it
    /// races this and either one may win.
    fn remove_instance<'a>(&'a self, instance: &'a InstanceId, node: &'a str) -> BoxFuture<'a, Result<()>>;
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
    /// `None` on a node that backs no drive with a local directory.
    ///
    /// Optional because a migration's *source* does not need a drive registry
    /// unless it actually has volumes to hand over — and an instance with no
    /// volumes at all (a stateless frontend, say) is perfectly happy to run,
    /// and therefore to move, on a node that declares no drives. Requiring one
    /// unconditionally strands exactly those moves: the driver skips the job
    /// on every poll and the migration sits in `Stopping` forever.
    pub registry: Option<&'a DriveRegistry>,
    pub engine: &'a dyn SyncEngine,
    pub control: &'a dyn InstanceControl,
    /// Where to publish live sync progress for `job.instance`, if anyone is
    /// watching. `None` in every test and in any caller that doesn't care.
    pub sync_progress: Option<crate::progress::MigrationSyncTracker>,
    /// Held for the span this instance is stopped and its volumes are moving
    /// (`Stopping`/`Syncing`), released as soon as the data has landed. A
    /// caller running several `MigrationJob`s from the same node concurrently
    /// shares one of these so at most one instance is ever down at a time,
    /// while the rest of each move (verify/switch/start/remove) proceeds
    /// unblocked in the background. A caller that only ever runs one job at a
    /// time can hand each its own fresh `Semaphore::new(1)`.
    pub transfer_gate: Arc<Semaphore>,
}

impl MigrationJob<'_> {
    /// The registry, for a step that genuinely needs one. Only reachable with
    /// a non-empty `volumes`, so a node that backs no drives never gets here.
    fn require_registry(&self) -> Result<&DriveRegistry> {
        self.registry.ok_or_else(|| DriveError::Transfer {
            message: format!("{} has volumes to move but this node backs no drives", self.instance),
        })
    }
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

    // The instance is down and the gate held for exactly `Stopping` and a
    // resumed `Syncing` (already stopped, data still moving after a crash) —
    // every later phase, including a resumed `Verifying` onward, runs with it
    // released. Acquired once up front rather than inside each arm, so a
    // `Syncing` resume holds it too without `Stopping` running again to grab it.
    let mut transfer_permit = match phase {
        MigrationPhase::Stopping | MigrationPhase::Syncing => {
            Some(job.transfer_gate.clone().acquire_owned().await.expect("transfer gate is never closed"))
        }
        _ => None,
    };

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
                // The data has landed: free the slot immediately so whichever
                // migration is queued behind this one can stop its own
                // container and start moving its volume right away, instead
                // of waiting on this instance's verify/switch/start/remove.
                drop(transfer_permit.take());
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
                // Best effort, and deliberately not a reason to fail: the
                // move is already committed by here — the target owns the
                // data and the assignment — so a source that will not let go
                // of a stopped container leaves clutter, not a broken
                // migration. Failing the job over it would report a move that
                // actually succeeded as `Failed` and invite someone to retry
                // it, which is far worse than one stale container.
                if let Err(e) = job.control.remove_instance(&job.instance, &job.source_node).await {
                    warn!(
                        instance = %job.instance,
                        node = %job.source_node,
                        error = %e,
                        "the move succeeded but the source container could not be removed",
                    );
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

/// Builds the closure `sync_all` hands to `ProgressSink` for one volume of
/// a migration: which volume this is (`volume_index`/`volume_count`) never
/// changes once the sync for it starts, so it's captured once here rather
/// than threaded through every `ProgressSink::report` call.
fn migration_progress_writer(
    tracker: crate::progress::MigrationSyncTracker,
    instance: InstanceId,
    volume_index: u64,
    volume_count: u64,
) -> crate::progress::ProgressSink {
    crate::progress::ProgressSink::new(move |files_done, files_total| {
        tracker.write().unwrap().insert(
            instance.clone(),
            crate::progress::MigrationSyncProgress { volume_index, volume_count, files_done, files_total },
        );
    })
}

async fn sync_all(node: &ClusterNode, job: &MigrationJob<'_>) -> Result<()> {
    let result = sync_all_inner(node, job).await;
    finish_sync(job, result)
}

async fn sync_all_inner(node: &ClusterNode, job: &MigrationJob<'_>) -> Result<()> {
    let volume_count = job.volumes.len() as u64;
    for (idx, volume) in job.volumes.iter().enumerate() {
        let state = node.state().await;
        let generation = state
            .volumes
            .get(volume)
            .ok_or_else(|| DriveError::VolumeNotFound { volume: volume.clone() })?
            .generation;

        let src_path = job.require_registry()?.volume_path(volume)?;
        let progress = match &job.sync_progress {
            Some(tracker) => {
                migration_progress_writer(tracker.clone(), job.instance.clone(), idx as u64 + 1, volume_count)
            }
            None => crate::progress::ProgressSink::none(),
        };
        job.engine.sync(&src_path, job.target_addr, volume, generation, None, progress).await?;

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

/// Clear `job.instance`'s live-progress entry unconditionally, whether
/// `sync_all_inner` finished cleanly or bailed out partway through the
/// volume loop. Called from exactly one exit point so no `?` inside the
/// loop can skip it and leave a stale "still syncing" entry behind for a
/// migration that actually failed.
fn finish_sync(job: &MigrationJob<'_>, result: Result<()>) -> Result<()> {
    if let Some(tracker) = &job.sync_progress {
        tracker.write().unwrap().remove(&job.instance);
    }
    result
}

async fn verify_all(job: &MigrationJob<'_>) -> Result<()> {
    for volume in &job.volumes {
        let src_path = job.require_registry()?.volume_path(volume)?;
        // `true` here means hashing every byte of the volume, the single most
        // expensive thing this crate does — never on a runtime thread.
        let source = scan::scan_off_runtime(src_path, true).await?;
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
    use std::sync::Arc;

    use super::*;

    struct AlwaysOk;
    impl InstanceControl for AlwaysOk {
        fn stop_instance<'a>(&'a self, _i: &'a InstanceId, _n: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn start_instance<'a>(&'a self, _i: &'a InstanceId, _n: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn remove_instance<'a>(&'a self, _i: &'a InstanceId, _n: &'a str) -> BoxFuture<'a, Result<()>> {
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

    #[test]
    fn migration_progress_writer_records_the_volume_position_and_file_counts() {
        let tracker: crate::progress::MigrationSyncTracker = Arc::default();
        let instance = InstanceId::new("site", "web", 0);
        let sink = migration_progress_writer(tracker.clone(), instance.clone(), 2, 3);

        sink.report(5, 12);

        let recorded = tracker.read().unwrap().get(&instance).copied().unwrap();
        assert_eq!(recorded.volume_index, 2);
        assert_eq!(recorded.volume_count, 3);
        assert_eq!(recorded.files_done, 5);
        assert_eq!(recorded.files_total, 12);
    }

    #[test]
    fn a_later_report_overwrites_the_instance_s_entry() {
        let tracker: crate::progress::MigrationSyncTracker = Arc::default();
        let instance = InstanceId::new("site", "web", 0);
        let sink = migration_progress_writer(tracker.clone(), instance.clone(), 1, 1);

        sink.report(1, 4);
        sink.report(4, 4);

        assert_eq!(tracker.read().unwrap().get(&instance).unwrap().files_done, 4);
    }

    struct UnusedEngine;
    impl SyncEngine for UnusedEngine {
        fn name(&self) -> &str {
            "unused"
        }
        fn sync<'a>(
            &'a self,
            _src: &'a std::path::Path,
            _peer: std::net::SocketAddr,
            _volume: &'a VolumeId,
            _generation: u64,
            _staging: Option<String>,
            _progress: crate::progress::ProgressSink,
        ) -> BoxFuture<'a, Result<crate::delta::SyncStats>> {
            unimplemented!("finish_sync tests never actually call sync")
        }
    }

    fn job_with_tracker(tracker: crate::progress::MigrationSyncTracker, instance: InstanceId) -> MigrationJob<'static> {
        MigrationJob {
            instance,
            source_node: "src".into(),
            target_node: "dst".into(),
            target_addr: "127.0.0.1:0".parse().unwrap(),
            volumes: Vec::new(),
            registry: None,
            engine: Box::leak(Box::new(UnusedEngine)),
            control: Box::leak(Box::new(AlwaysOk)),
            sync_progress: Some(tracker),
            transfer_gate: Arc::new(Semaphore::new(1)),
        }
    }

    // Regression test for the bug this module's cleanup wrapper fixes: a
    // `sync_all_inner` failure partway through the volume loop must not
    // leave the instance's progress entry stuck in the tracker forever,
    // reading as "still syncing" to anyone watching (the web UI) long after
    // the migration has actually failed.
    #[test]
    fn finish_sync_clears_the_tracker_entry_on_success_and_on_failure() {
        let instance = InstanceId::new("site", "web", 0);

        let tracker: crate::progress::MigrationSyncTracker = Arc::default();
        tracker.write().unwrap().insert(
            instance.clone(),
            crate::progress::MigrationSyncProgress { volume_index: 1, volume_count: 2, files_done: 0, files_total: 0 },
        );
        let job = job_with_tracker(tracker.clone(), instance.clone());
        let result = finish_sync(&job, Ok(()));
        assert!(result.is_ok());
        assert!(tracker.read().unwrap().get(&instance).is_none(), "success path must clear the tracker entry");

        // The failure path is what the bug actually broke: before the fix,
        // an early `?` return from the volume loop skipped the cleanup that
        // only ran after the loop, leaving this entry behind forever.
        let tracker: crate::progress::MigrationSyncTracker = Arc::default();
        tracker.write().unwrap().insert(
            instance.clone(),
            crate::progress::MigrationSyncProgress { volume_index: 1, volume_count: 2, files_done: 0, files_total: 0 },
        );
        let job = job_with_tracker(tracker.clone(), instance.clone());
        let err = DriveError::VolumeNotFound { volume: VolumeId::new("drive", "missing") };
        let result = finish_sync(&job, Err(err));
        assert!(result.is_err(), "finish_sync must hand back the original error");
        assert!(tracker.read().unwrap().get(&instance).is_none(), "failure path must also clear the tracker entry");
    }
}
