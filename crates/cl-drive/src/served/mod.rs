//! Served drives: one node holds the data and serves it to the rest of the
//! cluster over real NFSv4 — the kernel's own NFS server on the serving
//! side (`exports`), and a hand-built `mount(2)` client on every other
//! implementing node (`mount`). No FUSE, no bespoke wire protocol.
//!
//! `exports` manages this node's `exportfs` entries when it is a `served_by`
//! candidate; `mount` owns the client mounts (on demand, failover, repair).

pub mod exports;
pub mod failback;
pub mod mount;

pub use exports::{check_available as exports_available, ensure_export, remove_export, NFS_PORT};
pub use mount::{client_available, mount_permission, MountManager, MountSpec};
