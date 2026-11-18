//! Synchronous progress callback for `SyncEngine`, so the per-file send loop
//! in `delta.rs` never has to `.await` to report.

use std::sync::Arc;

type ProgressCallback = Arc<dyn Fn(u64, u64) + Send + Sync>;

#[derive(Clone, Default)]
pub struct ProgressSink(Option<ProgressCallback>);

impl ProgressSink {
    pub fn none() -> Self {
        Self(None)
    }

    pub fn new(f: impl Fn(u64, u64) + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(f)))
    }

    /// Unit is file counts, not bytes.
    pub fn report(&self, done: u64, total: u64) {
        if let Some(f) = &self.0 {
            f(done, total);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn none_reports_nothing_and_does_not_panic() {
        let sink = ProgressSink::none();
        sink.report(1, 2);
    }

    #[test]
    fn a_sink_forwards_done_and_total_to_its_callback() {
        let done = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let (d, t) = (done.clone(), total.clone());
        let sink = ProgressSink::new(move |done, total| {
            d.store(done, Ordering::SeqCst);
            t.store(total, Ordering::SeqCst);
        });
        sink.report(3, 7);
        assert_eq!(done.load(Ordering::SeqCst), 3);
        assert_eq!(total.load(Ordering::SeqCst), 7);
    }

    #[test]
    fn a_cloned_sink_calls_the_same_callback() {
        let calls = Arc::new(AtomicU64::new(0));
        let c = calls.clone();
        let sink = ProgressSink::new(move |_, _| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        let cloned = sink.clone();
        sink.report(0, 1);
        cloned.report(0, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}

use parking_lot::RwLock;
use std::collections::HashMap;

use cl_raft::ids::InstanceId;

/// A migrating instance's current volume and its file-count progress. Updated
/// from a synchronous callback, so the tracker uses a blocking lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationSyncProgress {
    /// 1-based: "syncing volume 2 of 3".
    pub volume_index: u64,
    pub volume_count: u64,
    pub files_done: u64,
    pub files_total: u64,
}

/// Shared with `cl-api` for live migration-sync progress (same pattern as
/// `cl_sched::PullTracker`).
pub type MigrationSyncTracker = Arc<RwLock<HashMap<InstanceId, MigrationSyncProgress>>>;
