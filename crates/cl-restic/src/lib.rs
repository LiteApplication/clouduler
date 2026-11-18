//! Restic-based backups of drive contents: destinations, per-drive
//! schedules/retention, and the restic invocations themselves.
//!
//! Distinct from `cl-backup`, which mirrors cluster metadata (config, stacks,
//! secrets) into git; nothing here touches that crate or vice versa.
//!
//! Restic always runs as a host binary (`std::process::Command`) against a
//! drive's real backing directory; see `dependency::restic_available`.
//!
//! A "node local" destination that is not the drive's holder gets a one-off sync
//! of the current data before restic runs locally, rather than a standing
//! replica: a live mirror (`cl_drive` replication) and point-in-time history are
//! different concerns, and a replica would cost ongoing traffic. That
//! orchestration lives in `clouduler`'s daemon, which has the docker and raft
//! access this crate deliberately lacks.

pub mod credentials;
pub mod dependency;
pub mod destination;
pub mod error;
pub mod restic;
pub mod restore;
pub mod retention;
pub mod schedule;
pub mod secret_names;

pub use error::{ResticError, Result};
