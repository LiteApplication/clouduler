//! Wake-ups for the daemon's periodic leader loops, so an operator pressing
//! "reconcile now" gets a pass immediately instead of at the next tick.
//!
//! A `Notify` stores one permit, so a nudge sent while a pass is already
//! running makes the loop run once more right after, never a pile of them.

use std::sync::Arc;

use tokio::sync::Notify;

#[derive(Clone, Default)]
pub struct Nudges {
    /// The Cloudflare DNS record reconcile loop.
    pub dns: Arc<Notify>,
    /// Certificate issuance and renewal.
    pub acme: Arc<Notify>,
    /// The git backup export loop.
    pub git_export: Arc<Notify>,
}
