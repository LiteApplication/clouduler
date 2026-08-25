//! Placement and the per-node reconcile loop.

pub mod error;
pub mod materialize;
pub mod place;
pub mod plan;
pub mod reconcile;
pub mod run;
pub mod spec;

pub use error::{Result, SchedError};
pub use materialize::{Materialized, Materializer};
pub use place::{candidates, place, Constraint, Decision, PlaceOutcome, Unplaceable};
pub use plan::{expand, Desired};
pub use reconcile::{diff, Action, PullTracker, Reconciler, Target, LABEL_REVISION};
pub use run::{desired_instances, Scheduler};
