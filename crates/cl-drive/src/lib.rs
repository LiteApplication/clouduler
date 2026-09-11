//! Drives and volumes: local implementations, delta replication, served
//! mounts and migration handoff.
//!
//! A drive is a cluster wide namespace; a node implements it by mapping the
//! drive name to a local directory (`registry`). A drive is either
//! replicated or served (`DriveRole`). A replicated drive's volumes are
//! addressed by `VolumeId` and kept in sync by walking them into a
//! `Manifest` (`scan`), diffing that against a peer, and shipping only what
//! changed over a small wire protocol (`transport`) using a rolling-hash
//! delta engine (`delta`); `sync` schedules that periodically. A served
//! drive has one copy: the serving node exports its real directory over
//! NFSv4 (`served::exports`), and every other implementing node mounts it
//! with a direct `mount(2)` call (`served::mount`). `migration` drives the
//! raft-recorded handoff when an instance moves nodes.

pub mod delta;
pub mod error;
pub mod migration;
pub mod progress;
pub mod registry;
pub mod scan;
pub mod served;
pub mod sync;
pub mod transport;

pub use delta::{BoxFuture, DeltaEngine, SyncEngine, SyncStats};
pub use error::{DriveError, Result};
pub use migration::{InstanceControl, MigrationJob};
pub use progress::{MigrationSyncProgress, MigrationSyncTracker, ProgressSink};
pub use registry::{resolve_in_root, resolve_relative, DriveRegistry, DriveRole};
pub use scan::{scan as scan_volume, Diff, FileMeta, Manifest};
pub use served::{
    client_available, ensure_export, exports_available, mount_permission, remove_export,
    MountManager, MountSpec, NFS_PORT,
};
pub use sync::{IntervalSource, StaticIntervals, SyncScheduler};
pub use transport::{drive_port, Message};
