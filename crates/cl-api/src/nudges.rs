//! Wake-ups for the daemon's periodic leader loops, so "reconcile now" runs a pass immediately. A `Notify` stores one permit,
//! so a nudge during a running pass makes the loop run once more, never a pile.

use std::sync::Arc;

use tokio::sync::Notify;

#[derive(Clone, Default)]
pub struct Nudges {
    pub dns: Arc<Notify>,
    pub acme: Arc<Notify>,
    pub git_export: Arc<Notify>,
    pub terraform: Arc<Notify>,
}
