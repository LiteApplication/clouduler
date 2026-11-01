//! What a destination is asked to deliver. Built by the engine in `cl-api`
//! from its replicated alert records; renderers never see raft types.

use serde::Serialize;

use crate::settings::Severity;

/// Where an alert stands in its lifecycle, from the receiver's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// First message for a stateful alert.
    Opened,
    /// Already open, got worse. `previous` is what was sent before.
    Escalated { previous: Severity },
    /// No longer happening.
    Resolved,
    /// A one-shot event with no recovery (`stack.deployed`, `log.match`).
    Event,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Link {
    pub label: String,
    /// Absolute URL.
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AlertMessage {
    /// Stable id of the alert record (same across opened/escalated/resolved).
    pub id: String,
    /// Catalog key, e.g. `instance.oom_killed`.
    pub event: String,
    /// Catalog category, e.g. `workloads`.
    pub category: String,
    pub severity: Severity,
    pub phase: Phase,
    /// One line, human: `media-radarr-1 update failed`.
    pub title: String,
    /// One or a few sentences.
    pub detail: String,
    /// The node, instance, volume or hostname it is about.
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Unix seconds the alert first opened (or the event happened).
    pub opened_at: i64,
    /// Unix seconds of this transition.
    pub at: i64,
    /// How many times a repeating event fired while this alert was open.
    pub count: u32,
    /// Page in the UI the alert is best looked at from. The first link is the
    /// primary one; renderers may show the others as secondary actions.
    pub links: Vec<Link>,
    /// Extra lines for a `log.match`: the matched log line(s), verbatim.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub excerpt: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum Body {
    Alert(AlertMessage),
    /// The day's info alerts in one message, oldest first.
    Digest {
        alerts: Vec<AlertMessage>,
    },
    /// From the "Send test" button.
    Test,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notification {
    /// `cluster.name`, shown as the sender.
    pub cluster: String,
    /// Base URL of the UI, no trailing slash, for the footer and the test.
    pub ui_url: Option<String>,
    pub body: Body,
}
