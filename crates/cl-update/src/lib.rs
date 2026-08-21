//! GitHub-release-based cluster self-update: orchestration and download logic.
//!
//! Split the way `cl-net::egress` is, pure logic apart from privileged glue:
//! `plan` and the pure functions in `orchestrator` are unit tested directly;
//! `release` and `install` need real network access or a live binary swap and
//! are gated behind `#[ignore]` for anything that actually exercises the
//! privileged half, matching how this repo gates its qemu-only tests.

pub mod install;
pub mod internal_client;
pub mod orchestrator;
pub mod peers;
pub mod plan;
pub mod release;
pub mod worker;
