//! Placement and the per-node reconcile loop.

pub mod deps;
pub mod error;
pub mod materialize;
pub mod place;
pub mod plan;
pub mod progress;
pub mod rebalance;
pub mod reconcile;
pub mod registries;
pub mod run;
pub mod spec;

pub use error::{Result, SchedError};
pub use materialize::{Materialized, Materializer};
pub use place::{candidates, place, Constraint, Decision, PlaceOutcome, Unplaceable};
pub use plan::{expand, Desired};
pub use progress::UpdateReporter;
pub use reconcile::{
    diff, pull_finished, pull_started, Action, PullTracker, Reconciler, Target, LABEL_REVISION,
};
pub use registries::{Registries, StoredLogin};
pub use run::{desired_instances, Scheduler};
