//! The migration handoff: `stop -> final sync -> verify -> switch -> start -> done`.
//!
//! Every step is recorded through raft around its work, so `run` can be called
//! again after a crash and resumes from the recorded phase. This module never
//! touches docker: `InstanceControl` (implemented by the daemon over
//! `cl-docker`) starts and stops the container, keeping migration testable with
//! a fake.
//!
//! Concurrent migrations off one node should share one
//! `MigrationJob::transfer_gate`: `run` holds it only while the instance is down
//! and its volumes are in flight (`Stopping`/`Syncing`) and releases it once the
//! data has landed, so the next migration can start while this one verifies,
//! switches and tidies up. At most one instance is down for its own move at a
//! time.

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

/// Starts and stops the container being moved. Implemented over `cl-docker`;
/// tests use a fake.
pub trait InstanceControl: Send + Sync {
    fn stop_instance<'a>(
        &'a self,
        instance: &'a InstanceId,
        node: &'a str,
    ) -> BoxFuture<'a, Result<()>>;
    fn start_instance<'a>(
        &'a self,
        instance: &'a InstanceId,
        node: &'a str,
    ) -> BoxFuture<'a, Result<()>>;
    /// Take the stopped container away on the node it was moved off. Must treat
    /// an already-absent container as success: the source's reconcile loop races
    /// this.
    fn remove_instance<'a>(
        &'a self,
        instance: &'a InstanceId,
        node: &'a str,
    ) -> BoxFuture<'a, Result<()>>;
}

pub struct MigrationJob<'a> {
    pub instance: InstanceId,
    pub source_node: String,
    pub target_node: String,
    pub target_addr: SocketAddr,
    pub volumes: Vec<VolumeId>,
    /// `None` on a node that backs no drive with a local directory. Optional
    /// because a source needs a registry only when it has volumes to hand over;
    /// requiring one stranded volume-less moves (stateless frontends) in
    /// `Stopping` forever.
    pub registry: Option<&'a DriveRegistry>,
    pub engine: &'a dyn SyncEngine,
    /// The cluster key for the verify step's own connection to the target.
    pub keys: cl_raft::KeyWatch,
    pub control: &'a dyn InstanceControl,
    /// Where to publish live sync progress for `job.instance`, if anyone watches.
    pub sync_progress: Option<crate::progress::MigrationSyncTracker>,
    /// Held while this instance is stopped and its volumes move
    /// (`Stopping`/`Syncing`), released once the data has landed. Concurrent
    /// jobs from one node share one so at most one instance is down at a time;
    /// a single-job caller can pass a fresh `Semaphore::new(1)`.
    pub transfer_gate: Arc<Semaphore>,
}

impl MigrationJob<'_> {
    /// For steps that need one; only reached with non-empty `volumes`.
    fn require_registry(&self) -> Result<&DriveRegistry> {
        self.registry.ok_or_else(|| DriveError::Transfer {
            message: format!(
                "{} has volumes to move but this node backs no drives",
                self.instance
            ),
        })
    }
}

/// Drive `instance`'s migration to `Done` or `Failed`, resuming from the phase
/// raft has recorded. A failure leaves the source assignment and volume primary
/// untouched, since `Switching` is only reached after every earlier step
/// succeeded.
pub async fn run(node: &ClusterNode, job: &MigrationJob<'_>) -> Result<()> {
    // Read once. `node.state()` lags an acknowledged write on non-leaders, so
    // re-reading per iteration returns the phase just left and reruns the step:
    // noisy for a stop, wrong for `Switching` (bumps every volume's generation).
    // The loop trusts the phase it just recorded; the read is for resuming only.
    let mut phase = recorded_phase(node, &job.instance).await?;

    // Gate held for `Stopping` and a resumed `Syncing` (stopped, data still
    // moving); later phases run with it released. Acquired up front so a
    // `Syncing` resume holds it without rerunning `Stopping`.
    let mut transfer_permit = match phase {
        MigrationPhase::Stopping | MigrationPhase::Syncing => Some(
            job.transfer_gate
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| DriveError::Transfer {
                    message: "migration transfer gate closed".into(),
                })?,
        ),
        _ => None,
    };

    loop {
        match phase {
            MigrationPhase::Stopping => {
                if let Err(e) = job
                    .control
                    .stop_instance(&job.instance, &job.source_node)
                    .await
                {
                    return Err(fail(node, &job.instance, e).await);
                }
                phase = advance(node, &job.instance, MigrationPhase::Syncing).await?;
            }

            MigrationPhase::Syncing => {
                if let Err(e) = sync_all(node, job).await {
                    return Err(fail(node, &job.instance, e).await);
                }
                // Data landed: free the slot so the next queued migration can start.
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
                // Point of no return: every volume's primary moves to the target.
                for volume in &job.volumes {
                    node.submit(Request::TouchVolume {
                        id: volume.clone(),
                        primary: job.target_node.clone(),
                    })
                    .await
                    .map_err(|e| DriveError::Transfer {
                        message: e.to_string(),
                    })?;
                }
                phase = advance(node, &job.instance, MigrationPhase::Starting).await?;
            }

            MigrationPhase::Starting => {
                if let Err(e) = job
                    .control
                    .start_instance(&job.instance, &job.target_node)
                    .await
                {
                    return Err(fail(node, &job.instance, e).await);
                }
                // Best effort, not a failure: the move is already committed, so a
                // source that won't release a stopped container leaves clutter, and
                // reporting `Failed` would invite a retry of a move that succeeded.
                if let Err(e) = job
                    .control
                    .remove_instance(&job.instance, &job.source_node)
                    .await
                {
                    warn!(
                        instance = %job.instance,
                        node = %job.source_node,
                        error = %e,
                        "the move succeeded but the source container could not be removed",
                    );
                }
                advance(node, &job.instance, MigrationPhase::Done).await?;
                // `Done` clears the migration record (state.rs).
                return Ok(());
            }

            MigrationPhase::Done => return Ok(()),

            MigrationPhase::Failed { reason } => {
                return Err(DriveError::Transfer { message: reason })
            }
        }
    }
}

/// The recorded phase raft has for `instance`, where a resumed migration picks up.
async fn recorded_phase(node: &ClusterNode, instance: &InstanceId) -> Result<MigrationPhase> {
    let state = node.snapshot().await;
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

/// Builds the closure `sync_all` hands `ProgressSink` for one volume; the
/// index/count are fixed once its sync starts, so they're captured here.
fn migration_progress_writer(
    tracker: crate::progress::MigrationSyncTracker,
    instance: InstanceId,
    volume_index: u64,
    volume_count: u64,
) -> crate::progress::ProgressSink {
    crate::progress::ProgressSink::new(move |files_done, files_total| {
        tracker.write().insert(
            instance.clone(),
            crate::progress::MigrationSyncProgress {
                volume_index,
                volume_count,
                files_done,
                files_total,
            },
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
        // Scoped so the shared snapshot is not held across the long sync below.
        let generation = {
            let state = node.snapshot().await;
            state
                .volumes
                .get(volume)
                .ok_or_else(|| DriveError::VolumeNotFound {
                    volume: volume.clone(),
                })?
                .generation
        };

        let src_path = job.require_registry()?.volume_path(volume)?;
        let progress = match &job.sync_progress {
            Some(tracker) => migration_progress_writer(
                tracker.clone(),
                job.instance.clone(),
                idx as u64 + 1,
                volume_count,
            ),
            None => crate::progress::ProgressSink::none(),
        };
        job.engine
            .sync(
                &src_path,
                job.target_addr,
                volume,
                generation,
                None,
                progress,
            )
            .await?;

        node.submit(Request::SetVolumeReplica {
            id: volume.clone(),
            node: job.target_node.clone(),
            generation,
        })
        .await
        .map_err(|e| DriveError::Transfer {
            message: e.to_string(),
        })?;
    }
    Ok(())
}

/// Clear `job.instance`'s progress entry whether `sync_all_inner` finished or
/// bailed. Called from one exit point so no `?` can leave a stale "still
/// syncing" entry for a failed migration.
fn finish_sync(job: &MigrationJob<'_>, result: Result<()>) -> Result<()> {
    if let Some(tracker) = &job.sync_progress {
        tracker.write().remove(&job.instance);
    }
    result
}

async fn verify_all(job: &MigrationJob<'_>) -> Result<()> {
    for volume in &job.volumes {
        let src_path = job.require_registry()?.volume_path(volume)?;
        // Hashing every byte is this crate's most expensive operation: never on a runtime thread.
        let source = scan::scan_off_runtime(src_path, true).await?;
        let target = transport::request_manifest(job.target_addr, volume, true, &job.keys).await?;

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

/// Record `phase` and return it, so the caller continues from what it wrote, not a lagging read.
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
        .map_err(|e| DriveError::Transfer {
            message: e.to_string(),
        })?;
    if let Some(err) = response.error {
        return Err(DriveError::Transfer { message: err });
    }
    Ok(phase)
}

/// Record the failure through raft (best effort) and return the original error.
async fn fail(node: &ClusterNode, instance: &InstanceId, err: DriveError) -> DriveError {
    if let Err(e) = node
        .submit(Request::AdvanceMigration {
            instance: instance.clone(),
            phase: MigrationPhase::Failed {
                reason: err.to_string(),
            },
        })
        .await {
        tracing::warn!(error = %e, "could not record the migration failure");
    }
    err
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    struct AlwaysOk;
    impl InstanceControl for AlwaysOk {
        fn stop_instance<'a>(
            &'a self,
            _i: &'a InstanceId,
            _n: &'a str,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn start_instance<'a>(
            &'a self,
            _i: &'a InstanceId,
            _n: &'a str,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn remove_instance<'a>(
            &'a self,
            _i: &'a InstanceId,
            _n: &'a str,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    // Full phase ordering needs raft and a transport listener (tests/migration.rs);
    // this checks the trait is object safe and easy to fake.
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

        let recorded = tracker.read().get(&instance).copied().unwrap();
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

        assert_eq!(tracker.read().get(&instance).unwrap().files_done, 4);
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

    fn job_with_tracker(
        tracker: crate::progress::MigrationSyncTracker,
        instance: InstanceId,
    ) -> MigrationJob<'static> {
        MigrationJob {
            instance,
            source_node: "src".into(),
            target_node: "dst".into(),
            target_addr: "127.0.0.1:0".parse().unwrap(),
            volumes: Vec::new(),
            registry: None,
            engine: Box::leak(Box::new(UnusedEngine)),
            keys: cl_raft::throwaway_key(),
            control: Box::leak(Box::new(AlwaysOk)),
            sync_progress: Some(tracker),
            transfer_gate: Arc::new(Semaphore::new(1)),
        }
    }

    // A `sync_all_inner` failure partway through must not leave the progress entry
    // stuck in the tracker (the web UI would show "still syncing" forever).
    #[test]
    fn finish_sync_clears_the_tracker_entry_on_success_and_on_failure() {
        let instance = InstanceId::new("site", "web", 0);

        let tracker: crate::progress::MigrationSyncTracker = Arc::default();
        tracker.write().insert(
            instance.clone(),
            crate::progress::MigrationSyncProgress {
                volume_index: 1,
                volume_count: 2,
                files_done: 0,
                files_total: 0,
            },
        );
        let job = job_with_tracker(tracker.clone(), instance.clone());
        let result = finish_sync(&job, Ok(()));
        assert!(result.is_ok());
        assert!(
            tracker.read().get(&instance).is_none(),
            "success path must clear the tracker entry"
        );

        // Failure path: an early `?` must not skip the cleanup.
        let tracker: crate::progress::MigrationSyncTracker = Arc::default();
        tracker.write().insert(
            instance.clone(),
            crate::progress::MigrationSyncProgress {
                volume_index: 1,
                volume_count: 2,
                files_done: 0,
                files_total: 0,
            },
        );
        let job = job_with_tracker(tracker.clone(), instance.clone());
        let err = DriveError::VolumeNotFound {
            volume: VolumeId::new("drive", "missing"),
        };
        let result = finish_sync(&job, Err(err));
        assert!(
            result.is_err(),
            "finish_sync must hand back the original error"
        );
        assert!(
            tracker.read().get(&instance).is_none(),
            "failure path must also clear the tracker entry"
        );
    }
}
