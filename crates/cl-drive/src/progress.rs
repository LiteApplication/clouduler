//! A plain callback for reporting "N of M done" from inside `SyncEngine`,
//! decoupled from *what* collects it. `delta.rs` calls `report` without
//! knowing whether anyone is listening; `migration.rs` (the one caller that
//! cares, for now) supplies a closure that writes into its own tracker.
//! Kept a synchronous `Fn` rather than an async callback so the per-file
//! send loop in `delta.rs` never has to `.await` just to report progress.

use std::sync::Arc;

type ProgressCallback = Arc<dyn Fn(u64, u64) + Send + Sync>;

#[derive(Clone, Default)]
pub struct ProgressSink(Option<ProgressCallback>);

impl ProgressSink {
    /// No one is listening — the cost of calling `report` is one branch.
    pub fn none() -> Self {
        Self(None)
    }

    pub fn new(f: impl Fn(u64, u64) + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(f)))
    }

    /// `done` out of `total`, in whatever unit the caller and its listener
    /// agreed on (this crate always uses file counts, not bytes).
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
