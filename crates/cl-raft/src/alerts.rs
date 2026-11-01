//! Replicated alert state: the in-app feed, the inbox nodes push signals
//! into, and the last delivery outcome per destination. The leader's engine
//! (`cl-api`) owns every write of records and health; any node may push a
//! signal.

use cl_config::AlertSeverity;
use serde::{Deserialize, Serialize};

/// Signals kept in the inbox. A leader that is away for long loses the oldest.
pub const MAX_ALERT_INBOX: usize = 500;
/// Hard cap of the feed `apply` enforces by itself (the engine prunes by age
/// and a softer cap long before this).
pub const MAX_ALERT_RECORDS: usize = 2500;
/// Deliveries remembered per record.
pub const MAX_RECORD_DELIVERIES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertState {
    /// Seen, but a warning that has not yet lasted the debounce.
    Pending,
    /// Open and (where severity allows) announced.
    Active,
    Resolved,
    /// A one-shot event: never opens or resolves.
    Event,
}

impl AlertState {
    pub fn is_open(self) -> bool {
        matches!(self, AlertState::Pending | AlertState::Active)
    }
}

/// A path in the UI (`/instances/gitlab-web-0`) and what to call it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertLink {
    pub label: String,
    pub path: String,
}

/// One attempt-series to one destination for one transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertDelivery {
    pub destination: String,
    pub at: i64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    /// `opened`, `escalated`, `resolved`, `event` or `digest`.
    #[serde(default)]
    pub phase: String,
    /// Provider-specific message identifier (e.g. Discord message ID)
    /// captured on delivery so future transitions can edit the message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

/// One entry of the in-app feed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertRecord {
    pub id: String,
    /// Catalog key.
    pub event: String,
    /// `event` plus the subject: two records with the same dedupe are the same
    /// problem. Open records are unique per dedupe.
    pub dedupe: String,
    pub category: String,
    pub severity: AlertSeverity,
    pub state: AlertState,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub opened_at: i64,
    pub last_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<i64>,
    #[serde(default)]
    pub count: u32,
    /// Severity this alert had before it got worse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalated_from: Option<AlertSeverity>,
    /// Highest severity already announced to destinations. Decides whether a
    /// new leader still owes a message, so a change of leader never re-sends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_severity: Option<AlertSeverity>,
    /// The resolved message went out (or was deliberately skipped).
    #[serde(default)]
    pub resolved_sent: bool,
    /// An info record that a daily digest already included.
    #[serde(default)]
    pub digested: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<AlertLink>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excerpt: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deliveries: Vec<AlertDelivery>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalAction {
    /// The condition holds (idempotent: repeated opens refresh it).
    Open,
    /// The condition is gone.
    Resolve,
    /// It happened once.
    Once,
}

/// Something a node saw that the leader's engine turns into a record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertSignal {
    /// Unique per signal (a uuid), so `TakeAlertSignals` removes exactly it.
    pub id: String,
    pub event: String,
    pub dedupe: String,
    pub action: SignalAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<AlertSeverity>,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub at: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excerpt: Vec<String>,
    /// For `Once`: how many occurrences this signal stands for.
    #[serde(default = "one")]
    pub count: u32,
}

fn one() -> u32 {
    1
}

/// Last delivery outcome of a destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestinationHealth {
    pub ok: bool,
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub consecutive_failures: u32,
}
