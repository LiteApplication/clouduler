//! Daemon subsystems: the bootstrap loop and every background job it spawns.

// The scheduler future's `Send` proof recurses through `join_all`'s nested
// future types and overruns the default limit of 128.
#![recursion_limit = "256"]

pub mod backup_restore;
pub mod backup_runner;
pub mod backup_secrets;
pub mod daemon;
pub mod drives;
pub mod egress;
pub mod expose;
pub mod internal_dns;
pub mod migration;
pub mod overlay;
pub mod placement;
pub mod proxy;
pub mod quorum_stamp;
pub mod relay;
pub mod restrict;
pub mod systemd;
pub mod volume_freeze;
