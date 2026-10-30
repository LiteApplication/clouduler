//! A read-only, docker-engine-compatible unix socket for image scanners.
//!
//! Grype and syft talk to "docker" only to inspect and save images. This
//! serves exactly those endpoints, from a pluggable `ImageBackend`, and
//! refuses the rest of the engine API.

mod backend;
mod route;
mod server;

pub use backend::{BackendError, BackendResult, ImageBackend, LocalBackend};
pub use route::{route, Refusal, Route};
pub use server::serve;
