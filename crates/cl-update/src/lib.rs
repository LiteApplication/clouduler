//! Git-based cluster self-update: orchestration and build logic.
//!
//! Split the way `cl-net::egress` is, pure logic apart from privileged glue:
//! `keypair`, `plan`, `peers` and the pure functions in `orchestrator` are
//! unit tested directly; `git`, `build` and `install` need a real git remote,
//! docker daemon, or a live binary swap and are gated behind `#[ignore]` for
//! anything that actually exercises the privileged half, matching how this
//! repo gates its qemu-only tests.

pub mod build;
pub mod git;
pub mod install;
pub mod internal_client;
pub mod keypair;
pub mod orchestrator;
pub mod peers;
pub mod plan;
pub mod worker;
