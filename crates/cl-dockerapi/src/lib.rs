//! A read-only, docker-engine-compatible unix socket for image scanners (grype,
//! syft), serving only the inspect and save endpoints from a pluggable
//! `ImageBackend` and refusing the rest of the engine API.

mod backend;
mod route;
mod server;

pub use backend::{BackendError, BackendResult, ImageBackend, LocalBackend};
pub use route::{route, Refusal, Route};
pub use server::serve;
