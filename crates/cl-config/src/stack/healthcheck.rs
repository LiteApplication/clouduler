//! Compose `healthcheck:` block.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Shape of the `test:` field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum HealthcheckTest {
    /// Run through the container's shell.
    Shell(String),
    /// `["CMD", ...]` or `["CMD-SHELL", "..."]` or `["NONE"]`.
    Exec(Vec<String>),
}

/// Compose `healthcheck:` block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Healthcheck {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<HealthcheckTest>,
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub interval: Option<Duration>,
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub timeout: Option<Duration>,
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub start_period: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disable: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_block() {
        let hc: Healthcheck = serde_yaml::from_str(
            r#"
test: ["CMD", "curl", "-f", "http://localhost/health"]
interval: 30s
timeout: 5s
retries: 3
start_period: 40s
"#,
        )
        .unwrap();
        assert_eq!(
            hc.test,
            Some(HealthcheckTest::Exec(vec![
                "CMD".into(),
                "curl".into(),
                "-f".into(),
                "http://localhost/health".into()
            ]))
        );
        assert_eq!(hc.interval, Some(Duration::from_secs(30)));
        assert_eq!(hc.retries, Some(3));
    }

    #[test]
    fn shell_string_form() {
        let hc: Healthcheck = serde_yaml::from_str("test: curl -f http://localhost/\n").unwrap();
        assert_eq!(
            hc.test,
            Some(HealthcheckTest::Shell("curl -f http://localhost/".into()))
        );
    }

    #[test]
    fn unknown_key_rejected() {
        let err = serde_yaml::from_str::<Healthcheck>("test: NONE\nbogus: 1\n").unwrap_err();
        assert!(err.to_string().contains("bogus"));
    }
}
