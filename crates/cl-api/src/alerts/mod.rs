//! Alerts: the leader's engine, the node-side producers and the settings
//! plumbing. What a message looks like and how it is delivered is `cl-alert`;
//! what is replicated is `cl_raft::alerts`.
//!
//! Everything here reads the settings (`_clouduler_alerts` and the sealed
//! destination secrets) out of the replicated state on every pass, so an edit
//! takes effect on the next tick on every node, with no restart.

pub mod conditions;
pub mod engine;
pub mod events;
pub mod filter;
pub mod lifecycle;
pub mod producers;
pub mod quorum;
pub mod route;
pub mod send;
pub mod settings;

use cl_alert::catalog;
use cl_alert::Severity;
use cl_config::AlertSeverity;
use cl_raft::alerts::{AlertSignal, SignalAction};
use cl_raft::{ClusterNode, Request};

use crate::server::AppState;

pub use engine::spawn;

pub fn sev(s: AlertSeverity) -> Severity {
    match s {
        AlertSeverity::Info => Severity::Info,
        AlertSeverity::Warning => Severity::Warning,
        AlertSeverity::Critical => Severity::Critical,
    }
}

pub fn sev_back(s: Severity) -> AlertSeverity {
    match s {
        Severity::Info => AlertSeverity::Info,
        Severity::Warning => AlertSeverity::Warning,
        Severity::Critical => AlertSeverity::Critical,
    }
}

pub(crate) fn unix_now() -> i64 {
    crate::mcp::problems::unix_now()
}

/// A signal skeleton for `event`; callers fill in the rest.
pub fn new_signal(event: &str, dedupe: String, action: SignalAction) -> AlertSignal {
    AlertSignal {
        id: uuid::Uuid::new_v4().to_string(),
        event: event.to_string(),
        dedupe,
        action,
        severity: catalog::find(event).map(|e| sev_back(e.severity)),
        title: catalog::find(event)
            .map(|e| e.title.to_string())
            .unwrap_or_default(),
        detail: String::new(),
        subject: String::new(),
        stack: None,
        instance: None,
        node: None,
        at: unix_now(),
        excerpt: Vec::new(),
        count: 1,
    }
}

/// Queue a signal for the leader's engine. Works from any node; failures
/// (no leader yet) are logged and dropped, since a signal that cannot be
/// delivered now is stale by the time it could be.
pub async fn push_signal(cluster: &ClusterNode, signal: AlertSignal) {
    if let Err(e) = cluster.submit(Request::PushAlertSignal(signal)).await {
        tracing::debug!(error = %e, "could not queue an alert signal");
    }
}

/// Base URL the UI is reachable at, for links in messages.
pub async fn ui_url(state: &AppState, settings: &cl_alert::AlertSettings) -> Option<String> {
    if let Some(url) = settings.public_url.as_deref() {
        let url = url.trim().trim_end_matches('/');
        if !url.is_empty() {
            return Some(url.to_string());
        }
    }
    let cfg = state.cluster_config.read().await;
    let fqdn = cfg.web_fqdn();
    if fqdn.is_empty() {
        return None;
    }
    Some(if cfg.web.through_proxy {
        format!("https://{fqdn}")
    } else {
        format!("https://{fqdn}:{}", cfg.web.port)
    })
}
