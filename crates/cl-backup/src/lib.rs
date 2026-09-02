//! Mirrors cluster config, stacks, configs and secrets into an admin-owned
//! git repository, and rebuilds a node's local state back out of one.
//!
//! Split into pure logic (this crate's bulk) plus a thin `git.rs` IO layer,
//! so the diffing/tree-building/crypto pieces are unit-testable without a
//! real repo.

pub mod age;
pub mod deploy_key;
pub mod error;
pub mod git;
pub mod snapshot;
pub mod status;
pub mod tree;

pub use error::{BackupError, Result};
