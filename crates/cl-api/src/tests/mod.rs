//! Route level tests: a real single node raft cluster, a lazily connected
//! docker client (never actually dialed, since none of the routes exercised
//! here touch it), driven through the router with `tower::ServiceExt::oneshot`.

mod common;
mod mcp;
mod oauth_flow;

mod auth;
mod config;
mod cluster;
mod renames;
mod stacks;
mod instances;
mod internal_auth;
mod api_shapes;
mod secrets;
mod terraform;

pub(crate) use common::*;
