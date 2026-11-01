//! `x-clouduler.alerts`: per stack (and per service) control over the alerts
//! a stack raises, plus log line watches.
//!
//! ```yaml
//! x-clouduler:
//!   alerts: false            # nothing about this stack is ever sent
//! ```
//!
//! ```yaml
//! x-clouduler:
//!   alerts:
//!     mute: [instance.unhealthy, "update.*"]
//!     severity:
//!       instance.exited: info
//!     logs:
//!       - match: "FATAL"
//!         severity: critical
//!       - regex: "panic: .*"
//!         title: "Go panic"
//!         cooldown: 30m
//! ```
//!
//! The same block under a service's `x-clouduler` applies to that service's
//! containers only and is merged over the stack's: `enabled` and `severity`
//! entries override, `mute` and `logs` add up.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Mirrors `cl_alert::Severity`; `cl-config` stays free of that crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawAlerts", into = "RawAlerts")]
pub struct StackAlerts {
    /// `alerts: false`. Log rules still parse but never run.
    pub enabled: bool,
    /// Catalog keys, or a category prefix ending in `.*` (`instance.*`).
    pub mute: Vec<String>,
    pub severity: BTreeMap<String, AlertSeverity>,
    pub logs: Vec<LogRule>,
}

impl Default for StackAlerts {
    fn default() -> Self {
        Self {
            enabled: true,
            mute: Vec::new(),
            severity: BTreeMap::new(),
            logs: Vec::new(),
        }
    }
}

/// Raise `log.match` when a container prints a matching line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogRule {
    /// Plain substring. Exactly one of `match` and `regex`.
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub literal: Option<String>,
    /// Rust `regex` syntax, matched anywhere in the line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub ignore_case: bool,
    #[serde(default = "default_log_severity")]
    pub severity: AlertSeverity,
    /// Alert title. Defaults to `<service> logged "<pattern>"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// After a match, further matches of this rule on the same container are
    /// only counted, not raised, for this long.
    #[serde(default = "default_cooldown", with = "humantime_serde")]
    pub cooldown: Duration,
    #[serde(default, skip_serializing_if = "is_default_stream")]
    pub stream: LogStream,
    /// At stack level: only these services. Empty means all. Not allowed on
    /// a service's own rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    #[default]
    Both,
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct AlertsMap {
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    mute: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    severity: BTreeMap<String, AlertSeverity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    logs: Vec<LogRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum RawAlerts {
    Switch(bool),
    Map(AlertsMap),
}

impl TryFrom<RawAlerts> for StackAlerts {
    type Error = String;

    fn try_from(raw: RawAlerts) -> Result<Self, String> {
        Ok(match raw {
            RawAlerts::Switch(enabled) => StackAlerts {
                enabled,
                ..StackAlerts::default()
            },
            RawAlerts::Map(m) => StackAlerts {
                enabled: m.enabled,
                mute: m.mute,
                severity: m.severity,
                logs: m.logs,
            },
        })
    }
}

impl From<StackAlerts> for RawAlerts {
    fn from(a: StackAlerts) -> Self {
        if a.mute.is_empty() && a.severity.is_empty() && a.logs.is_empty() {
            return RawAlerts::Switch(a.enabled);
        }
        RawAlerts::Map(AlertsMap {
            enabled: a.enabled,
            mute: a.mute,
            severity: a.severity,
            logs: a.logs,
        })
    }
}

fn default_true() -> bool {
    true
}
fn is_true(b: &bool) -> bool {
    *b
}
fn is_false(b: &bool) -> bool {
    !*b
}
fn default_log_severity() -> AlertSeverity {
    AlertSeverity::Warning
}
fn default_cooldown() -> Duration {
    Duration::from_secs(5 * 60)
}
fn is_default_stream(s: &LogStream) -> bool {
    *s == LogStream::Both
}

/// Longest a log pattern may be. A line is checked against every pattern of a
/// container, so an absurd one would only slow the node down.
pub const MAX_LOG_PATTERN_LEN: usize = 512;
/// Most log rules one container may carry (stack and service rules together).
pub const MAX_LOG_RULES: usize = 32;

impl StackAlerts {
    /// This stack block with a service's block merged over it: `enabled` and
    /// `severity` entries of the service win, `mute` and `logs` add up.
    /// Rules limited to other services by `services` are left out.
    pub fn merged(stack: Option<&StackAlerts>, service: Option<&StackAlerts>, name: &str) -> Self {
        let mut out = StackAlerts::default();
        if let Some(s) = stack {
            out.enabled = s.enabled;
            out.mute = s.mute.clone();
            out.severity = s.severity.clone();
            out.logs = s
                .logs
                .iter()
                .filter(|r| r.services.is_empty() || r.services.iter().any(|n| n == name))
                .cloned()
                .collect();
        }
        if let Some(s) = service {
            out.enabled = s.enabled;
            for m in &s.mute {
                if !out.mute.contains(m) {
                    out.mute.push(m.clone());
                }
            }
            out.severity
                .extend(s.severity.iter().map(|(k, v)| (k.clone(), *v)));
            out.logs.extend(s.logs.iter().cloned());
        }
        out
    }

    /// Whether the catalog key `event` is muted by this block.
    pub fn mutes(&self, event: &str) -> bool {
        !self.enabled || self.mute.iter().any(|m| mute_matches(m, event))
    }

    /// Severity override for `event`, if any: an exact key wins over a
    /// `prefix.*` entry.
    pub fn severity_for(&self, event: &str) -> Option<AlertSeverity> {
        if let Some(s) = self.severity.get(event) {
            return Some(*s);
        }
        self.severity
            .iter()
            .filter(|(k, _)| mute_matches(k, event))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, s)| *s)
    }

    /// Syntax checks. `at` is the path in the stack file, `service_level`
    /// forbids `services` on rules.
    pub fn validate(&self, at: &str, service_level: bool) -> Result<(), crate::ConfigError> {
        use crate::ConfigError;
        for (i, key) in self.mute.iter().enumerate() {
            if !well_formed_key(key) {
                return Err(ConfigError::invalid(
                    format!("{at}.mute[{i}]"),
                    format!("{key:?} is not an alert key like instance.unhealthy or instance.*"),
                ));
            }
        }
        for key in self.severity.keys() {
            if !well_formed_key(key) {
                return Err(ConfigError::invalid(
                    format!("{at}.severity.{key}"),
                    format!("{key:?} is not an alert key like instance.unhealthy or instance.*"),
                ));
            }
        }
        if self.logs.len() > MAX_LOG_RULES {
            return Err(ConfigError::invalid(
                format!("{at}.logs"),
                format!("at most {MAX_LOG_RULES} log rules"),
            ));
        }
        for (i, rule) in self.logs.iter().enumerate() {
            let at = format!("{at}.logs[{i}]");
            let pattern = match (&rule.literal, &rule.regex) {
                (Some(p), None) | (None, Some(p)) => p,
                _ => {
                    return Err(ConfigError::invalid(
                        at,
                        "set exactly one of match and regex",
                    ))
                }
            };
            if pattern.is_empty() {
                return Err(ConfigError::invalid(at, "the pattern is empty"));
            }
            if pattern.len() > MAX_LOG_PATTERN_LEN {
                return Err(ConfigError::invalid(
                    at,
                    format!("the pattern is longer than {MAX_LOG_PATTERN_LEN} characters"),
                ));
            }
            if let Some(re) = &rule.regex {
                if let Err(e) = regex::RegexBuilder::new(re).size_limit(1 << 20).build() {
                    return Err(ConfigError::invalid(
                        format!("{at}.regex"),
                        format!("not a valid regular expression: {e}"),
                    ));
                }
            }
            if rule.cooldown < Duration::from_secs(1) {
                return Err(ConfigError::invalid(
                    format!("{at}.cooldown"),
                    "must be at least 1s",
                ));
            }
            if service_level && !rule.services.is_empty() {
                return Err(ConfigError::invalid(
                    format!("{at}.services"),
                    "only allowed on the stack-level alerts block",
                ));
            }
        }
        Ok(())
    }
}

/// `instance.unhealthy` or `instance.*` (a prefix ending in `.*`).
pub fn well_formed_key(key: &str) -> bool {
    let body = key.strip_suffix(".*").unwrap_or(key);
    !body.is_empty()
        && body
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
}

/// Whether the pattern (`instance.*` or an exact key) covers `event`.
pub fn mute_matches(pattern: &str, event: &str) -> bool {
    match pattern.strip_suffix(".*") {
        Some(prefix) => event
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('.')),
        None => pattern == event,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> StackAlerts {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn mute_patterns_match_keys_and_prefixes() {
        assert!(mute_matches("instance.*", "instance.unhealthy"));
        assert!(!mute_matches("instance.*", "instances.x"));
        assert!(!mute_matches("instance.*", "instance"));
        assert!(mute_matches("log.match", "log.match"));
        assert!(!mute_matches("log.match", "log.matches"));
        assert!(well_formed_key("instance.*"));
        assert!(well_formed_key("self_update.rolled_back"));
        assert!(!well_formed_key("*"));
        assert!(!well_formed_key("Instance.x"));
        assert!(!well_formed_key(".x"));
    }

    #[test]
    fn service_block_merges_over_the_stack_block() {
        let stack = parse("{mute: [update.*], severity: {instance.exited: info}, logs: [{match: A}, {match: B, services: [web]}]}");
        let svc = parse("{mute: [instance.unhealthy], severity: {instance.exited: critical}, logs: [{regex: 'x+'}]}");
        let merged = StackAlerts::merged(Some(&stack), Some(&svc), "db");
        assert!(merged.enabled);
        assert_eq!(merged.mute, vec!["update.*", "instance.unhealthy"]);
        assert_eq!(
            merged.severity_for("instance.exited"),
            Some(AlertSeverity::Critical)
        );
        // `B` is for the web service only.
        assert_eq!(merged.logs.len(), 2);
        let web = StackAlerts::merged(Some(&stack), None, "web");
        assert_eq!(web.logs.len(), 2);
        assert!(merged.mutes("update.failed"));
        assert!(!merged.mutes("instance.exited"));
    }

    #[test]
    fn the_service_switch_overrides_the_stack_switch() {
        let off = parse("false");
        let on = parse("true");
        assert!(!StackAlerts::merged(Some(&off), None, "web").enabled);
        assert!(StackAlerts::merged(Some(&off), Some(&on), "web").enabled);
        assert!(!StackAlerts::merged(Some(&on), Some(&off), "web").enabled);
        assert!(StackAlerts::merged(None, None, "web").enabled);
    }

    #[test]
    fn validation_catches_bad_rules() {
        let ok = parse("{logs: [{match: FATAL}, {regex: 'panic: .*'}], mute: ['a.*']}");
        assert!(ok.validate("alerts", false).is_ok());
        for bad in [
            "{logs: [{}]}",
            "{logs: [{match: A, regex: B}]}",
            "{logs: [{regex: '('}]}",
            "{logs: [{match: ''}]}",
            "{mute: ['Bad']}",
            "{logs: [{match: A, cooldown: 0s}]}",
        ] {
            assert!(parse(bad).validate("alerts", false).is_err(), "{bad}");
        }
        let scoped = parse("{logs: [{match: A, services: [web]}]}");
        assert!(scoped.validate("alerts", false).is_ok());
        assert!(scoped.validate("alerts", true).is_err());
    }
}
