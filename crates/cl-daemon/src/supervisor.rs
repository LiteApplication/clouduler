//! A named set of background tasks that are torn down together.
//!
//! Replaces loose `tokio::spawn` handles: a panic inside a task is logged
//! with the task's name instead of vanishing with the dropped handle, a loop
//! that is safe to rerun can be restarted with a backoff, and dropping the
//! set (or `abort_all`) stops everything in it.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use futures::FutureExt;
use tokio::task::JoinSet;
use tracing::error;

const RESTART_BACKOFF_START: Duration = Duration::from_secs(1);
const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct Supervisor {
    tasks: JoinSet<()>,
}

impl Supervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `fut` once. A panic is logged under `name`; the task is not restarted.
    pub fn spawn<F>(&mut self, name: &'static str, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.tasks.spawn(async move {
            if let Err(panic) = AssertUnwindSafe(fut).catch_unwind().await {
                error!(task = name, panic = %panic_message(&panic), "background task panicked");
            }
        });
    }

    /// Run the future `make` builds; if it panics, log it and build a fresh one
    /// after an exponentially growing delay. Only for loops that are safe to
    /// start over. A future that returns normally ends the task.
    pub fn spawn_restarting<F, Fut>(&mut self, name: &'static str, mut make: F)
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.tasks.spawn(async move {
            let mut backoff = RESTART_BACKOFF_START;
            loop {
                match AssertUnwindSafe(make()).catch_unwind().await {
                    Ok(()) => return,
                    Err(panic) => {
                        error!(
                            task = name,
                            panic = %panic_message(&panic),
                            ?backoff,
                            "background task panicked, restarting"
                        );
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(RESTART_BACKOFF_MAX);
                    }
                }
            }
        });
    }

    pub fn abort_all(&mut self) {
        self.tasks.abort_all();
    }
}

/// Log a failure that cannot be acted on at the call site, instead of
/// discarding it with `let _ =`.
pub fn log_err<T, E: std::fmt::Display>(what: &str, result: Result<T, E>) {
    if let Err(e) = result {
        tracing::warn!(error = %e, "{what}");
    }
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}
