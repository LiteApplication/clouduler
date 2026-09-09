//! Daemon subsystems: the bootstrap loop and every background job it spawns.

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
