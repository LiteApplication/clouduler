//! GitHub-release-based cluster self-update: orchestration and download logic.
//!
//! `plan` and the pure functions in `orchestrator` are unit tested; the
//! privileged halves of `release` and `install` are gated behind `#[ignore]`.

pub mod install;
pub mod internal_client;
pub mod orchestrator;
pub mod peers;
pub mod plan;
pub mod release;
pub mod reset;
pub mod rollback;
pub mod worker;
