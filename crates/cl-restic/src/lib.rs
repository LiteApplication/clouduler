//! Restic-based backups of drive contents: destinations to back up to,
//! schedules/retention per drive, and the restic invocations themselves.
//!
//! This is a different concern from `cl-backup`, which mirrors *cluster
//! metadata* (config, stacks, secrets) into a git repository - nothing here
//! ever touches that crate, and nothing there touches volume data.
//!
//! Restic always runs as a host binary (`std::process::Command`), never in a
//! container, directly against a drive's real backing directory - see
//! `dependency::restic_available` for the startup check that this binary is
//! actually on `PATH`.
//!
//! A "node local" destination that isn't already the drive's holder gets a
//! one-off sync of the current data before restic runs against it locally,
//! rather than being added as a standing replica of the drive: a live mirror
//! (`cl_drive`'s replication) and a point-in-time backup history are
//! different concerns, and conflating them would mean every backup
//! destination permanently costs ongoing replication traffic. That
//! orchestration - the sync, stopping/starting containers, actually
//! invoking restic - lives in `clouduler`'s own daemon code, which has the
//! docker and raft access this crate deliberately doesn't.

pub mod dependency;
pub mod destination;
pub mod error;
pub mod restic;
pub mod restore;
pub mod retention;
pub mod schedule;
pub mod secret_names;

pub use error::{ResticError, Result};
