//! Placement and the per-node reconcile loop.

pub mod error;
pub mod place;
pub mod plan;
pub mod reconcile;

pub use error::{Result, SchedError};
pub use place::{candidates, place, Constraint, Decision};
pub use plan::{expand, Desired};
pub use reconcile::{diff, Action, Reconciler, Target, LABEL_REVISION};
