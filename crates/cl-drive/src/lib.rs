//! Drives and volumes: local implementations, delta replication and migration handoff.
//!
//! A drive is a cluster wide namespace; a node implements it by mapping the
//! drive name to a local directory (`registry`). A volume is a subdirectory
//! of a drive, addressed by `VolumeId` and bind-mounted into containers.
//! Volumes are kept in sync by walking them into a `Manifest` (`scan`),
//! diffing that against a peer, and shipping only what changed over a small
//! wire protocol (`transport`) using a rolling-hash delta engine
//! (`delta`). `sync` schedules that periodically per volume; `migration`
//! drives the raft-recorded handoff when an instance moves nodes.

pub mod delta;
pub mod error;
pub mod migration;
pub mod registry;
pub mod scan;
pub mod sync;
pub mod transport;

pub use delta::{BoxFuture, DeltaEngine, SyncEngine, SyncStats};
pub use error::{DriveError, Result};
pub use migration::{InstanceControl, MigrationJob};
pub use registry::DriveRegistry;
pub use scan::{scan as scan_volume, Diff, FileMeta, Manifest};
pub use sync::{IntervalSource, StaticIntervals, SyncScheduler};
pub use transport::{drive_port, Message};
