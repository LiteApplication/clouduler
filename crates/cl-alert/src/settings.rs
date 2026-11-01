//! Alert settings: where alerts go and how loud they are.
//!
//! Stored as JSON in the internal cluster config object [`SETTINGS_CONFIG`]
//! (the same pattern as registries and static routes), edited from the UI.
//! Everything secret lives in sealed cluster secrets named by
//! [`url_secret`], [`auth_secret`] and [`SMTP_PASSWORD_SECRET`], never here.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Internal config object holding [`AlertSettings`] as JSON.
pub const SETTINGS_CONFIG: &str = "_clouduler_alerts";
/// Sealed SMTP password, when the server wants one.
pub const SMTP_PASSWORD_SECRET: &str = "_clouduler_alert_smtp_password";
const SECRET_PREFIX: &str = "_clouduler_alert_";

/// Sealed webhook URL of a webhook or discord destination. A discord URL
/// carries its token, so the whole URL is the secret.
pub fn url_secret(destination: &str) -> String {
    format!("{SECRET_PREFIX}{destination}_url")
}

/// Sealed `Authorization` header value of a generic webhook destination.
pub fn auth_secret(destination: &str) -> String {
    format!("{SECRET_PREFIX}{destination}_auth")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertSettings {
    /// Master switch for outbound deliveries. In-app alerts are always kept.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// A warning is only sent once it has lasted this long. Critical alerts
    /// go out immediately; info never interrupts anyone.
    #[serde(default = "default_debounce", with = "humantime_serde")]
    pub warning_debounce: Duration,
    /// Local time (`HH:MM`) the daily digest of info alerts is sent.
    #[serde(default = "default_digest_at")]
    pub digest_at: String,
    /// IANA zone `digest_at` is read in (`Europe/Paris`). Absent means the
    /// leader's system zone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Send a new message when an open alert gets worse (warning to critical).
    #[serde(default = "default_true")]
    pub renotify_on_escalation: bool,
    /// Send a message when an alert that was delivered resolves.
    #[serde(default = "default_true")]
    pub notify_resolved: bool,
    /// Display warnings in the UI immediately without waiting for the debounce.
    /// If they resolve before the debounce expires, they are deleted without a trace.
    #[serde(default)]
    pub display_pending_warnings: bool,
    /// How long resolved and one-shot alerts are kept in the in-app feed.
    #[serde(default = "default_retention", with = "humantime_serde")]
    pub retention: Duration,
    /// Base URL used for links in messages, e.g. `https://cloud.example.com`.
    /// Absent means the cluster's web address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// Always serialised, even empty: the UI reads it unconditionally.
    #[serde(default)]
    pub destinations: Vec<Destination>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp: Option<SmtpSettings>,
    /// Per-event overrides, keyed by catalog event key (`instance.unhealthy`).
    #[serde(default)]
    pub events: BTreeMap<String, EventOverride>,
    #[serde(default)]
    pub thresholds: Thresholds,
}

impl Default for AlertSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            warning_debounce: default_debounce(),
            digest_at: default_digest_at(),
            timezone: None,
            renotify_on_escalation: true,
            notify_resolved: true,
            display_pending_warnings: false,
            retention: default_retention(),
            public_url: None,
            destinations: Vec::new(),
            smtp: None,
            events: BTreeMap::new(),
            thresholds: Thresholds::default(),
        }
    }
}

/// One place alerts are sent to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Destination {
    /// Unique, `[a-z0-9-]`, shown in lists and at the top of each message.
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(flatten)]
    pub kind: DestinationKind,
    /// Severities sent the moment they fire (warnings after the debounce).
    #[serde(default = "default_severities")]
    pub severities: BTreeSet<Severity>,
    /// Also send the daily digest of info alerts here.
    #[serde(default)]
    pub digest: bool,
    /// Only alerts in these catalog categories. Empty means all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    /// Only alerts about these stacks (cluster-wide alerts always pass).
    /// Empty means all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stacks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DestinationKind {
    /// JSON POST to any URL (ntfy, Gotify, Home Assistant, your own service).
    /// URL in [`url_secret`], optional `Authorization` value in [`auth_secret`].
    Webhook,
    /// Discord channel webhook. URL in [`url_secret`].
    Discord(DiscordOptions),
    /// Email through [`AlertSettings::smtp`].
    Email { to: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordOptions {
    #[serde(default)]
    pub style: DiscordStyle,
    /// Who a `ping` notifies: `<@&role id>`, `<@user id>`, `@here` or
    /// `@everyone`. Without it, `ping` behaves like `normal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mention: Option<String>,
    #[serde(default)]
    pub notify: DiscordNotifyLevels,
    /// Link buttons under the message (open in Clouduler, logs).
    #[serde(default = "default_true")]
    pub buttons: bool,
    /// When an alert resolves, edit the original alert message in-place
    /// instead of sending a new message.
    #[serde(default)]
    pub edit_resolved: bool,
    /// Overrides the webhook's own name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

impl Default for DiscordOptions {
    fn default() -> Self {
        Self {
            style: DiscordStyle::default(),
            mention: None,
            notify: DiscordNotifyLevels::default(),
            buttons: true,
            edit_resolved: false,
            username: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscordStyle {
    /// A coloured embed with fields.
    #[default]
    Embed,
    /// One plain line of text.
    Short,
}

/// How a message announces itself in Discord.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscordNotify {
    /// Mention [`DiscordOptions::mention`].
    Ping,
    /// A regular message: unread badge and the channel's usual notification.
    Normal,
    /// `SUPPRESS_NOTIFICATIONS`: lands in the channel without a push.
    Silent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordNotifyLevels {
    #[serde(default = "ping")]
    pub critical: DiscordNotify,
    #[serde(default = "normal")]
    pub warning: DiscordNotify,
    #[serde(default = "silent")]
    pub info: DiscordNotify,
    /// Resolved messages.
    #[serde(default = "silent")]
    pub resolved: DiscordNotify,
}

impl Default for DiscordNotifyLevels {
    fn default() -> Self {
        Self {
            critical: DiscordNotify::Ping,
            warning: DiscordNotify::Normal,
            info: DiscordNotify::Silent,
            resolved: DiscordNotify::Silent,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmtpSettings {
    pub host: String,
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    #[serde(default)]
    pub security: SmtpSecurity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// `Clouduler <alerts@example.com>` or a bare address.
    pub from: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmtpSecurity {
    /// Plain connection upgraded with STARTTLS (port 587).
    #[default]
    Starttls,
    /// TLS from the first byte (port 465).
    Tls,
    /// No encryption, for a relay on a trusted network only.
    None,
}

/// Overrides for one catalog event.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<Route>,
}

/// Where an event goes instead of its severity's default routing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// Not recorded, not sent.
    Muted,
    /// Recorded in-app, never sent anywhere.
    InApp,
    /// In-app plus exactly these destinations, ignoring their severity filter.
    Destinations(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Thresholds {
    /// Percent used on a drive's filesystem that raises `node.disk_low`.
    #[serde(default = "default_disk_warn")]
    pub disk_warning_percent: u8,
    #[serde(default = "default_disk_crit")]
    pub disk_critical_percent: u8,
    /// Percent of RAM in use, sustained for `memory_for`.
    #[serde(default = "default_mem")]
    pub memory_percent: u8,
    #[serde(default = "default_mem_for", with = "humantime_serde")]
    pub memory_for: Duration,
    /// Days before expiry that raise `cert.expiring` as a warning / critical.
    #[serde(default = "default_cert_warn")]
    pub cert_warning_days: u32,
    #[serde(default = "default_cert_crit")]
    pub cert_critical_days: u32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            disk_warning_percent: default_disk_warn(),
            disk_critical_percent: default_disk_crit(),
            memory_percent: default_mem(),
            memory_for: default_mem_for(),
            cert_warning_days: default_cert_warn(),
            cert_critical_days: default_cert_crit(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_debounce() -> Duration {
    Duration::from_secs(5 * 60)
}
fn default_digest_at() -> String {
    "09:00".to_string()
}
fn default_retention() -> Duration {
    Duration::from_secs(30 * 24 * 3600)
}
fn default_severities() -> BTreeSet<Severity> {
    BTreeSet::from([Severity::Critical, Severity::Warning])
}
fn default_smtp_port() -> u16 {
    587
}
fn ping() -> DiscordNotify {
    DiscordNotify::Ping
}
fn normal() -> DiscordNotify {
    DiscordNotify::Normal
}
fn silent() -> DiscordNotify {
    DiscordNotify::Silent
}
fn default_disk_warn() -> u8 {
    90
}
fn default_disk_crit() -> u8 {
    95
}
fn default_mem() -> u8 {
    92
}
fn default_mem_for() -> Duration {
    Duration::from_secs(10 * 60)
}
fn default_cert_warn() -> u32 {
    14
}
fn default_cert_crit() -> u32 {
    7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_keep_the_lists_the_ui_reads() {
        let v = serde_json::to_value(AlertSettings::default()).unwrap();
        assert_eq!(v["destinations"], serde_json::json!([]));
        assert_eq!(v["events"], serde_json::json!({}));
    }

    #[test]
    fn every_destination_kind_round_trips() {
        let json = serde_json::json!({
            "destinations": [
                {"name": "hook", "kind": "webhook", "severities": ["critical"]},
                {"name": "disc", "kind": "discord", "style": "short", "mention": "@here", "severities": ["critical", "warning"]},
                {"name": "mail", "kind": "email", "to": ["a@b.example"]},
            ]
        });
        let s: AlertSettings = serde_json::from_value(json).expect("parse");
        assert_eq!(s.destinations.len(), 3);
        assert!(matches!(s.destinations[0].kind, DestinationKind::Webhook));
        assert!(matches!(
            s.destinations[1].kind,
            DestinationKind::Discord(_)
        ));
        let back = serde_json::to_value(&s).unwrap();
        let again: AlertSettings = serde_json::from_value(back).expect("reparse");
        assert_eq!(s, again);
    }

    #[test]
    fn unknown_destination_fields_still_parse_without_losing_the_rest() {
        let s: AlertSettings = serde_json::from_value(serde_json::json!({
            "destinations": [{"name": "x", "kind": "webhook"}]
        }))
        .unwrap();
        assert_eq!(s.destinations[0].name, "x");
    }
}
